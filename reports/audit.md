# Xylo audit — agent framework, flows and defects (2026-10-30)

Scope: the whole workspace (13 crates, ~134k lines of application Rust, plus the vendored Hermes engine where Xylo calls into it). Every non-test source file in `engine`, `runtime`, `services`, `capabilities`, `desktop`, `worker`, `helper`, `helper-client`, `scheduler` was read in full; tests were skimmed for intent. `cargo check --locked --workspace` passes with 9 dead-code warnings. No application code was changed.

Severity: **C** critical (data loss, security, or the product is unusable), **H** high, **M** medium, **L** low. "PLAUSIBLE" marks a mechanism verified in code whose trigger frequency could not be confirmed.

---

## 1. Verdict in one page

Xylo's backend is more rigorous than Claude Code or Codex on durability and secrets: every tool call is a journaled action with an idempotency key, interrupted writes become `outcome_unknown` and must be reconciled, secrets live only in the OS keyring, MCP servers and shell commands run inside the Anthropic sandbox runtime, WASM plugins have a real capability boundary. None of that exists in the CLIs.

The product loop is behind both CLIs, and several bugs undermine the rigor:

1. **The agent's identity and prompt are not Xylo's.** Hermes prepends "You are Hermes Agent, created by Nous Research" and a per-minute timestamp before the Xylo system prompt, and injects keyword-triggered "finalizer" boilerplate into answers. The timestamp also defeats prompt caching on every turn.
2. **Permission mode is not honored once a turn exists.** The mode is frozen into the turn request; queued turns can only be tightened at admission, never loosened. Hooks are classified as writes, so every hook event is an approval prompt and any PreToolUse hook breaks read-only tasks.
3. **Parallel edits race.** Hermes runs tool batches 8-wide; `file_edit` has no per-file lock, so two edits to one file in a single model response silently lose one.
4. **Durability has a hole at the busiest point.** The oldest store paths (`complete_turn`, `begin_action`, `register_user_turn`) use deferred SQLite transactions shared between two processes, so a finished turn can fail with "database is locked".
5. **The transcript misrepresents what happened.** Final answers jump above their tool cards, partial text disappears on error, cancel leaves no marker, unfinished cards spin forever, host status messages are never rendered, subagent work is interleaved into the parent with no grouping, and the context ring shows a number that is not the context size.
6. **No fast switching.** Model picking requires the provider editor round trip; discovering models for any connection silently activates it; "always allow" rules die when you switch chats because the idle worker is killed.

Counts: 6 critical, 15 high, 40 medium, 38 low (99 findings) across the six areas.

---

## 2. Architecture and flow graphs

See the companion page (published artifact) for the drawn graphs. Text form:

### 2.1 Process topology

```
GPUI desktop (crates/desktop)  ──HostCommand/HostEvent (async_channel 128/256)──▶  HostState on "xylo-host" thread (crates/runtime/host.rs)
                                                                                      │  one xylo-worker PROCESS per chat session (JSONL over stdin/stdout, Envelope{request_id,sequence})
                                                                                      ▼
                                                                         xylo-worker (crates/worker) ── HermesEngine (crates/engine)
                                                                                      │       ├─ ToolBroker (journal, approvals, hooks, connectors, capabilities)
                                                                                      │       ├─ GuardedProvider (tiktoken budget, summary compaction, local lease)
                                                                                      │       ├─ AttachmentProvider → NativeProvider/Bedrock (HTTP)
                                                                                      │       └─ hermes_agent::AgentLoop::run_stream (vendored)
                                                                                      ▼
                                              SQLite xylo.db (WAL) ◀── shared by host AND worker ──▶   OS keyring (host AND worker)
                                              xylo-helper process (per workspace, assets/plugins/environments)   Node sandbox-helper (@anthropic-ai/sandbox-runtime) wraps every shell/MCP child
```

### 2.2 One turn (send → Completed)

```
composer Enter → Xylo::submit → submit_persisted_draft → host.submit_draft (try_send)
  host: prepare_command reserves queued[session] → handle → worker_command_with_queue_receipt
        validate provider → resolve credential (keyring) → retire_idle_workers (kills other chats' idle workers)
        → resources.acquire → spawn_worker if none → tighten(permissions) → durable admit (drafts::admit_turn)
        → publish DraftAdmitted → stdin: RunAuthorized{turn, credential}
  worker: register_user_turn → spawn task → HermesEngine::run
        completed_turn replay check → ToolBroker::new (definitions, advertised filter) → unresolved_actions gate
        → GuardedProvider(AttachmentProvider(NativeProvider)) → restored_messages (checkpoint, re-dispatch pending tool calls!)
        → build_registry → AgentConfig{max_turns 200, external_context_management} → save_checkpoint → Started
        → AgentLoop::run_stream:
             build_system_prompt = HERMES IDENTITY + Xylo prompt + hermes guidance + timestamp
             loop { guidance callback (steering) → max_turns? → stream model call
                    (GuardedProvider: lease → fit_with_summary [may call summarizer inline] → HTTP SSE)
                    → deltas (32ms batch task) → tool calls? no → finished
                    → JoinSet(8) execute → ToolBroker::execute_scoped
                         journal lookup → ReadOnly check → change_preview → begin_action → ToolStarted
                         → approval? (writes && !preapproved) → ApprovalRequired → poll 100ms
                         → mark_running → PreToolUse hooks → run → finish_action → PostToolUse hooks → ToolFinished
                    → on_checkpoint → loop guard → context pressure status }
        → complete_turn (checkpoint + assistant message + receipt, one tx) → Completed{text, usage=SUM over calls}
  host reader: accepts(active request_id) → notices → active=false → HostEvent::Engine → settle_turn_queue
  UI: TurnState::apply → Completed pushes assistant message ABOVE tool cards
```

### 2.3 Approval

```
execute_scoped: definition.writes && !(mode preapproves || remembered(rule))
  → ApprovalGate.install(id, sync_channel) → emit ApprovalRequired{rule label}
  → loop { cancel? → deny ; rx.recv_timeout(100ms) }
UI card → decide_action → WorkerCommand::Approve{action_id, allow, remember} → host send_to(session) → stdin
worker gate.decide_with → remembered.insert(rule) (worker memory only) → sender.send(allow)
late Approve → worker Error with the Approve's request_id → host drops it (accepts only active Run id)
```

### 2.4 Stop / cancel

```
UI Stop/Esc → Cancel → host capture_stop: turn_queue.stop (pauses whole queue), cancel queued admission
  → if a run was only queued: synthetic Cancelled to UI (but admission may still dispatch → DraftAdmitted → second Cancelled)
  → else try_send(Cancel) to worker; kill ONLY if the command channel is full
worker: task.cancel.cancel() → engine interrupt_task → hermes interrupt; GuardedProvider select! on cancel
  → finish_interrupted_generation → Cancelled (partial text stays only in the checkpoint, never in messages)
```

### 2.5 Permission mode

```
settings.permissions ──(copied at send)──▶ TurnRequest.permissions ──▶ ToolBroker reads ONLY this
queued turn admission: permissions = tighter(captured, current)   // loosen ignored, tighten applied
running turn: never re-read. UI label: "Applies to new tasks".
```

### 2.6 Context budget

```
GuardedProvider per run (summaries NOT durable): tiktoken exact (OpenAI) or ×1.3
fixed = tool schemas + 256 + 64/tool; allowance = context − fixed − output
view > 80% → summarized_view → summarize_history (inline model call, provider timeout) → "# Summary of earlier work"
hard overflow of one pending tool round → terminal error for the turn
UI ring = Completed.usage.input_tokens (sum of prompt tokens over every call in the turn) / context_tokens
```

---

## 3. The findings that matter most

| # | Sev | Area | Finding | Where |
|---|-----|------|---------|-------|
| 1 | C | Engine | Hermes identity "You are Hermes Agent… created by Nous Research" is prepended before "You are Xylo"; no SOUL.md override is seeded. | vendor hermes-agent `methods_prompt_llm.rs:88-100`, `context.rs:400,879` |
| 2 | C | Engine | `with_timestamp` puts "Conversation started: … %I:%M %p" in the system block, so the cached prefix changes every minute; defeats the Anthropic `cache_control` breakpoint and OpenAI prefix caching; persisted into checkpoints. | `context.rs:930-942`, `engine/providers/request.rs:306` |
| 3 | C | Engine | Two `file_edit`/`file_write` calls to one file in one response run in parallel (JoinSet, concurrency 8) with no per-file lock; second atomic rename overwrites the first; both report success and the second approval diff is computed from stale content. | `runtime_state.rs:656-662`, `methods_tools_background.rs:801`, `engine/tools/code.rs:290-312`, `builtin.rs:251-272` |
| 4 | C | Services | `complete_turn`, `begin_action`, `register_user_turn`, recovery paths use `BEGIN DEFERRED` read-then-write across two processes; `busy_timeout` is not honored on lock upgrade → "database is locked" turns a finished turn into an Error. Newer modules already use `Immediate`. | `services/store.rs:293,398,423,504,517,644,697`, `extension_receipts.rs:45`, `learning/persistence.rs` |
| 5 | C | Extensions | Hooks run as `extension_component` (writes:true, no approval rule, not pre-approved by any mode) → every PreToolUse/PostToolUse firing is an ApprovalRequired, twice per tool call. | `engine/tools/mod.rs:861-941`, `portable.rs:15` |
| 6 | C | Extensions | Any PreToolUse hook in Read-only mode fails every tool ("requires write permission"). | `engine/tools/mod.rs:528-531, 681-687` |
| 7 | H | Permissions | Mode change never reaches a running turn; queued turns only tighten (`tighter(captured, current)`). User-facing: "even if I change them the agent doesn't honor them". | `host.rs:2641,2666`, `turn_queue_control.rs:440,90-104`, `turn_queue.rs:188` |
| 8 | H | Permissions | `run_command` remember rule drops leading flags: approving `bash -c "cargo test"` with "always allow" creates rule `run:bash` → later `bash -c "curl … \| sh"` is auto-approved. Same for `sh -c`, `python -c`, `node -e`. | `engine/tools/mod.rs:80-91,634-636` |
| 9 | H | Host | "Always allow" rules live in the worker's memory; `retire_idle_workers` kills every idle worker when another chat runs → rules vanish on chat switch, never persisted. | `host.rs:903-916`, `tools/mod.rs:33-62` |
| 10 | H | Host | Stop sends a cooperative Cancel only; escalates to kill only if the 64-slot channel is full. A hung worker leaves the session "already working" until app restart. | `turn_queue_control.rs:207-213`, `host.rs:2532-2537` |
| 11 | H | Engine | Budget/guard exits become opaque `Error{retryable:true}`: max 200 calls (model's closing summary discarded), 3 invalid tool-name batches, or 4 consecutive turns where every call failed (a failing `cargo test` loop). | `engine.rs:511-522`, `worker/main.rs:264-270`, `runtime_state.rs:704-729` |
| 12 | H | Engine | Hermes keyword hints are not gated by `native_product_finalization`: "fix … why" → "REPO_RESEARCH_PLAN: complete" contract; "http/urls/browse" → "write WEB_SEARCH_USED: no"; repo-review tool narrowing runs unconditionally. | `methods_run_stream.rs:137-160,1015-1028`, `session_intent.rs` |
| 13 | H | Chat UI | Quit never checks running turns or pending approvals; worker gets Shutdown 1s/6s/kill; partial answers and approvals lost silently. Quit also becomes impossible when a draft cannot flush (host dead): only "Keep working". | `xylo.rs:684-720`, `drafts.rs:1016-1064`, `host.rs:3181-3203` |
| 14 | H | Settings UI | Provider save failure (keyring) leaves a dangling `credential_ref`, clears the key field, and the relabelled form lets a retry persist a provider with no secret → every turn fails "credential is missing". No way to delete a provider or its secret. | `providers.rs:101-149,288-303`, `xylo.rs:1708-1757` |
| 15 | H | Settings UI | Discovering models for any connection sets `settings.provider` to it (silent activation). | `providers.rs:837-856` |
| 16 | H | Settings UI | A blocked `outcome_unknown` action makes the chat permanently unusable (recovery gate never clears; only escape is a new chat). | `recovery.rs:724-731,427-429`, `xylo.rs:1395` |
| 17 | H | Context | Context ring = `usage.input_tokens` from Completed, which is the sum of prompt tokens across every model call in the turn, not the context size; can exceed 100%. | `chat.rs:1351-1365`, hermes `merge_usage` |
| 18 | H | Chat UI | Host `Status` events are assigned to a field no render path reads: "Connecting…", "Ready", "Discovered N models", "Guidance added", "Recovered N interrupted actions" are never shown. | `xylo.rs:1263` |
| 19 | H | Extensions | `plugin_run` accepts model-supplied `capabilities` (`read_workspace`, `https:<host>`) for any workspace `.wasm` → self-granted read+exfiltration path; approval shows raw args. | `capabilities/plugins.rs:181-187`, `plugins/runtime.rs:595-633` |
| 20 | H | Secrets | Custom provider header blocklist misses `cookie`, `proxy-authorization`, `x-auth-token`… → plaintext secrets in settings JSON and turn-queue snapshots. | `services/providers/mod.rs:407-420` |
| 21 | H | Credentials | Keyring prompts: unsigned/ad-hoc builds change identity each build; two binaries (app + worker) each open the keyring; no in-memory cache; seven construction sites. | `services/credentials.rs`, `engine.rs:106`, `tools/mod.rs:159`, `image_generation.rs:116`, `portable.rs:72` |

---

## 4. Full defect list by area

### 4.1 Engine (crates/engine, worker, Hermes loop)

| Sev | Finding | Where |
|-----|---------|-------|
| C | Hermes identity prefix before Xylo prompt (3.1) | `methods_prompt_llm.rs:88-100` |
| C | Per-minute timestamp in system block busts caching (3.2) | `context.rs:930-942` |
| C | Parallel edit lost-update race (3.3) | `code.rs:290-312` |
| H | Guard/budget exits as opaque retryable errors (3.11) | `engine.rs:511-522` |
| H | Ungated Hermes finalizer hints pollute answers (3.12) | `methods_run_stream.rs:137-160` |
| H | `run:<program>` remember rule when first arg is a flag (3.8) | `tools/mod.rs:80-91` |
| M | Fuzzy `repair_tool_call` substring-matches over all registered (including non-advertised) tools; nondeterministic (`write` → `file_write` or `todo_write`); misses count toward the 3-strike limit | `methods_tools_background.rs:23-50`, `engine.rs:594-619` |
| M | Retry policy shallow: stream retry only before the first delta, 2 retries, max wait 30 s; a 429 with `Retry-After: 60` fails the turn | `methods_prompt_llm.rs:1118-1142`, `config.rs:649`, `providers/mod.rs:254-258` |
| M | Context summarizer runs inline on the provider call path with the full provider timeout as its only deadline (observed "Condensing earlier context… 6m 04s") | `provider_guard.rs:300-358` |
| M | Summaries are per-run memory; every later turn over 80% re-summarizes; the checkpoint summary-restore code is dead | `engine.rs:161-171,314`, `provider_guard.rs:55` |
| M | `WorkerCommand::Steer` is a no-op; durable guidance applies only at loop boundaries, invisible during approval waits, long tools or long streams | `worker/main.rs:298-309`, `services/steering.rs:314-318` |
| M | Empty/reasoning-only responses are retried through the non-streaming path, so the final answer arrives only in Completed with no deltas | `methods_run_stream.rs:471-479` |
| M | Every engine failure is `retryable: true`; the UI cannot tell "reconnect provider" from "try again" | `worker/main.rs:264-270` |
| M | Child tool cards and approvals are emitted on the parent's channel with no parent id | `tools/mod.rs:166-177` |
| M | `initial_definitions` picks up to 8 extra tools by checking whether a tool-name prefix appears in the prompt text; brittle discovery | `tools/mod.rs:347-359` |
| L | CRLF file + single-line `old_string` + multi-line `new_string` writes mixed line endings | `code.rs:204-215` |
| L | Deltas (32 ms async batch) can arrive after `ToolStarted` (blocking_send); final flush dropped if channel full | `engine.rs:761-781`, `tools/mod.rs:1021-1025` |
| L | Resume re-dispatches un-journaled tool calls before `Started`; `Started` clears UI cards; runs under the new mode | `engine.rs:326,459,669-671`, `state.rs:56-60` |
| L | Windows: `shell_command`/`run_command` always `NotStarted` although PowerShell is wired and advertised | `sandbox.rs:66-69`, `code.rs:617-632` |
| L | Approval event carries the full diff plus the content twice; a ~2 MiB write with heavy escaping can exceed the 8 MiB line limit and kill the worker's writer task (PLAUSIBLE) | `builtin.rs:263-272`, `worker/main.rs:64-67` |
| L | AcceptEdits auto-applies `.git/hooks/*`, `.cargo/config.toml`, `.vscode/tasks.json` (PLAUSIBLE; Claude Code shares this) | `sandbox.rs:449-457` |
| L | `RenewableProvider::prepare` does a host round trip on every model call; >4 in flight → "renewal is busy" (parent + 2 delegates + summarizer) | `renewal.rs:99-106`, `worker/credentials.rs:89-111` |

### 4.2 Runtime host (crates/runtime)

| Sev | Finding | Where |
|-----|---------|-------|
| H | Stop never escalates to kill (3.10) | `turn_queue_control.rs:207-213` |
| H | Remembered rules lost on chat switch (3.9) | `host.rs:903-916` |
| H | Permission tighten-only at admission (3.7) | `host.rs:2641,2666` |
| M | Worker's Error reply to a late/bad Approve (and its Cancel ack) carries the Approve's request_id; the host forwards only the active Run's id → silently dropped, card never settles | `host.rs:2964-2973`, `worker/main.rs:321-339` |
| M | `workers` mutex held up to 8 s while reaping a dead worker; `capture_stop` uses `try_lock` → Stop answered with "Select a single active task" | `host.rs:2596-2607`, `turn_queue_control.rs:113-115` |
| M | Stop while a run is queued for admission in a session with an active worker: synthetic Cancelled to UI, active run untouched (PLAUSIBLE) | `turn_queue_control.rs:176-214` |
| M | `settle_turn_queue` bypasses the `operations` gate; can spawn a worker during `install_update` (PLAUSIBLE, narrow race) | `host.rs:3059-3067,3092-3100` |
| M | `HostCommand::Workspace` is awaited inline on the single intake loop behind the settings commit chain; a keyring prompt in `SaveProvider` blocks opening a project | `host.rs:515-519`, `settings_control.rs:25-31` |
| M | Partial assistant text never persisted on cancel/crash; after reload the user message has no reply | `engine.rs:181-209`, `store.rs:384-400` |
| M | Queue pauses on every Stop and on restart; every queued item then needs manual resume; reason never shown in UI | `host.rs:2406`, `desktop/turn_queue.rs:266` |
| L | `active=false` before the terminal event is sent → a new Send can be admitted before the previous Completed reaches the UI | `host.rs:3034-3058` |
| L | Host-synthesized events use `sequence: 0` and ad-hoc request ids; sequence never validated | `host.rs:2421-2431,2511-2521,3121-3132` |
| L | `Store::call` uses `try_send` on a 128-slot queue → "Database queue is full" can fail `complete_turn` (PLAUSIBLE) | `store.rs:210-224` |
| L | Worker stderr logged verbatim at debug; verbose `RUST_LOG` could carry tokens from upstream HTTP logs (PLAUSIBLE) | `host.rs:2906-2918` |
| L | One worker per session, cold start on every chat switch (`Store::open`, `HermesEngine::new`) | `host.rs:2829` |
| L | Scheduled turns run `AskForWrites` in a hidden occurrence session; approvals block there with only a notification as cue | `host.rs:3321-3329` |
| L | `Hello`/`Ready` handshake unused; protocol mismatch surfaces as a terminal Error on first Run | `worker/main.rs` |

### 4.3 Services, persistence and credentials (crates/services)

| Sev | Finding | Where |
|-----|---------|-------|
| C | Deferred transactions on cross-process hot paths (3.4) | `store.rs:293,398,423,…` |
| H | Header blocklist incomplete → plaintext secrets (3.20) | `providers/mod.rs:407-420` |
| H | Keyring prompts: identity changes per unsigned build, app and worker both open the keyring, no cache (3.21) | `services/credentials.rs` |
| M | Every MCP tool is classified as a write, so read-only MCP tools always hit the approval gate | `mcp.rs:264,300` |
| M | 4096-stripe execution leases: id collision between concurrent qualified actions → "This action is still running" tool failure | `action_review/leases.rs:78-84,118`, `tools/mod.rs:578` |
| M | Embedding failures swallowed; model reload attempted on every call; semantic memory silently off; `ort::init_from` is process-global so reload cannot switch libraries (PLAUSIBLE) | `memory.rs:103-139,336,368` |
| M | Lifetime caps count tombstones and nothing purges: after 16 384 lifetime queued turns "Queue receipt storage is full" forever; same for guidance records and fact links | `turn_queue.rs:135-138`, `steering.rs:222-223`, `memory_facts.rs:150-171` |
| M | Re-submitted guidance after a cancel is answered with the stale Cancelled record (PLAUSIBLE) | `steering.rs:184-200` |
| M | Guidance arriving before the turn's first checkpoint is rejected as `AgentError::Io` (PLAUSIBLE) | `steering.rs:305-308` |
| M | Connector OAuth refresh has no single-flight; rotating refresh tokens can be revoked | `connectors/oauth.rs:113-141` |
| L | MCP children SIGKILLed per turn; no `shutdown`/`notifications/cancelled`; respawn + sandbox init every turn | `tools/mod.rs:1166`, `mcp.rs:39-48,264,300,329-337` |
| L | MCP stdio stderr is `/dev/null`; sandbox/Node failures surface as "MCP initialization timed out" | `mcp.rs:182` |
| L | Migrations: one `user_version=3` batch, no ALTER path; module-level detection by `sql.contains("6291456")` | `store.rs:766-793`, `memory_forms.rs:54` |
| L | Deleted facts keep link rows; `document_drafts` cursor set without `has_more`; embedding stored twice; history search "best hit" is the newest message; `expires_in > 86400` rejected at sign-in | various |

### 4.4 Chat UI (crates/desktop: xylo.rs, chat.rs, state.rs, history, sidebar)

| Sev | Finding | Where |
|-----|---------|-------|
| H | Quit ignores running turns; quit impossible when drafts cannot flush (3.13) | `xylo.rs:684-720` |
| H | `Xylo.status` write-only; host status never rendered (3.18) | `xylo.rs:1263` |
| H | Context ring shows summed prompt tokens (3.17) | `chat.rs:1351-1365` |
| M | `settle_tools` only clears `approval`; a card without `ToolFinished` spins "in progress" forever after Error/crash | `state.rs:150-155`, `chat.rs:488-494` |
| M | Streamed text rendered only while `running` or status "Cancelled" → on Error the partial text disappears while the card says "Progress so far is saved." | `chat.rs:329` |
| M | `Completed` pushes the assistant message into `messages`, which render above `turn.tools` → the answer jumps above its tool cards; chronology inverted; narration is one block after all tools | `xylo.rs:1326-1333`, `chat.rs:200-345` |
| M | Cancel leaves no "Stopped" marker; partial text unlabelled and never persisted | `chat.rs:339-345` |
| M | `Error.retryable` ignored everywhere; no Retry/regenerate/edit-last; single global error slot gets overwritten by the next autosave error | `chat.rs:703-745` |
| M | Sidebar filter: every keystroke sets `page = HistorySearch`; `search_open` never set false | `history_search.rs:66-67`, `sidebar.rs:234-240` |
| M | Bare Escape bound window-wide to DenyOrStop (denies oldest approval or cancels the turn) with no confirmation (PLAUSIBLE) | `shortcuts.rs:41` |
| M | No client-side timeout on submission: "Sending message" spinner, Send disabled, Stop hidden until `Started` → locked chat if `DraftAdmitted` never arrives | `drafts.rs:780-822`, `chat.rs:1536-1548` |
| M | Delegate card shows "Tool result recorded. Open the attached result or action journal" (8 KB presentation cap) and a raw JSON preview; subagent tool rows interleaved into the parent list | `tools/mod.rs` `bound_result`, `tool_presentation.rs:435` |
| M | Each `todo_write` leaves its own "Updated plan N of M" row | `chat.rs:301,1745-1785` |
| L | Enter silently no-ops during import/flush/conflict; `new_chat` closes the work panel (Cmd-N hides the terminal); `product_error` substring mapping rewrites unrelated errors and promises "reconnecting" that never happens; session-less envelopes attributed to the open chat; `to_string_pretty` per render of approval cards; full markdown re-parse of accumulated text per delta batch; empty assistant row on tool-only turns; `WorkspaceUnavailable` leaves "loading project…"; `HostEvent::File` forces the Files page | `xylo.rs:1400-1405,1465,2064-2093,1297,973-977,1181-1200`, `chat.rs:617,336` |

### 4.5 Settings and secondary surfaces (crates/desktop/xylo/*)

| Sev | Finding | Where |
|-----|---------|-------|
| H | Dangling `credential_ref` after save failure (3.14) | `providers.rs:101-149` |
| H | Discovery activates the connection (3.15) | `providers.rs:837-856` |
| H | Blocked interrupted action = dead chat (3.16) | `recovery.rs:724-731` |
| M | Opening an unscoped saved draft sets `terminal = None` (kills PTYs) without confirmation and skips workspace-switch hooks | `drafts.rs:1426-1444` |
| M | "Close terminal and stop its shell" (Updates) bypasses the close confirmation; cached shells for other projects are killed silently | `updates.rs:267-283` |
| M | Quit silently no-ops when any draft/document/memory flush is in error or conflict; the reason is visible only on that page | `xylo.rs:676-690` |
| M | No way to remove a provider or its keyring secret | `providers.rs` |
| M | Portable extensions install and enable with no permission review; hooks enabled before configured; Remove without confirmation | `extensions.rs:321-334,396-421` |
| L | Render path mutates state and enqueues host IO (`image_provider_settings_page`); raw service errors reach the banner unmapped; recovery gate silently bypassed when the 32-entry review cache is full; asset poll error freezes progress; image model may be saved blank (PLAUSIBLE); connector removal without confirm (PLAUSIBLE) | `image_generation.rs:377-381`, `recovery.rs:189-209`, `assets.rs:96-101` |

### 4.6 Extensions, sandbox, capabilities (crates/capabilities, services/extensions, helper, scheduler)

| Sev | Finding | Where |
|-----|---------|-------|
| C | Hooks = approval per event (3.5); PreToolUse breaks read-only (3.6) | `tools/mod.rs:861-941` |
| H | `plugin_run` self-granted WASM capabilities (3.19) | `plugins.rs:181-187` |
| M | Environment capture ledger grows forever (≤16 384 reservations, 32 MiB each until publish); after the cap every `environment_execute` fails with no reclaim control | `environments/capture.rs:192-197,229,745-751` |
| M | Sandbox setup failure exit 125 is indistinguishable from a tool exiting 125; write-classified shell commands become `outcome_unknown` though nothing ran | `run.mjs:43-44`, `child-entry.mjs:14`, `process.rs:163-168` |
| M | Sandbox launcher trusts whatever `node` is on PATH or in Homebrew and runs unsandboxed with helper privileges; `helper_path` falls back to the compile-time `CARGO_MANIFEST_DIR` in release | `sandbox.rs:405-425,128-133` |
| L | Native helper subprocesses (ssh/docker/llama-server/whisper) inherit the full app environment including `*_API_KEY`; sandboxed children correctly `env_clear` | `capabilities/process.rs:66-76`, `helper-client/lib.rs:123-137` |
| L | Scheduler is at-least-once (5 min lease re-claim); `schedule_create` is an agent-callable write tool, so the model can schedule recurring prompts for itself | `scheduler/lib.rs:215-243,341-347` |
| L | Cross-process admission polls every 500 ms with no fairness; `metadata_lock` gives up after 2 s and surfaces as a tool error | `resources/shared.rs:49-78,206-213` |
| L | Package update keeps `enabled`; skill bodies change without re-review; `allowed-tools` patterns (`Bash(git:*)`, `Task`) fail only at fork time; a `SessionStart` entry marks the whole component unsupported while its PreToolUse entries still run; `staging/recovery-*` never cleaned; `remote_execute` dispatchable with an agent-supplied executable path | `catalog.rs:89`, `manifest.rs:259-290`, `runtime.rs:746-759`, `assets/mod.rs:738-746`, `lib.rs:184` |

Verified clean: zip-slip and symlink handling in asset and plugin archives; download integrity (pinned SHA-256 and size, HTTPS only, resume re-hashes); workspace reads via `openat`/`O_NOFOLLOW`; document copies no-clobber and receipt-recoverable; WASM boundary (no WASI, fuel/memory/table limits); sandbox `env_clear` + allowlist; SigV4; ChatGPT OAuth (PKCE, nonce, JWKS, constant-time state); memory scope isolation; no lock held across `.await`; no worker zombies; `HostClient` never blocks the UI thread; no panicking `unwrap` on the UI path.

---

## 5. Flow and product problems (your screenshots, in order)

### 5.1 Transcript ("worst, should be like Claude")

What the code does: `chat_row` renders `messages` (persisted) first, then `turn.tools` as a flat list under one "Working through the task" eyebrow, then the streamed text. Consequences you saw:

- Forty tool rows stack before any text; completed batches never collapse.
- The narration shows up once at the bottom, then jumps above the tools on Completed.
- Subagent rows are interleaved because child brokers share the parent's event channel; the Delegate card body is the 8 KB-capped placeholder.
- Every `todo_write` leaves a row; a failed Find becomes a large framed card.
- No "Thinking…" or elapsed indicator between tool batches; `Xylo.status` is never rendered; the only mover is the status line from Hermes, mapped to six canned strings.

Claude Code's transcript is a chronological stream: text, then a collapsed tool group ("Read 3 files, Searched 2 patterns…"), then more text, with one live spinner line ("Thinking… 12s · esc to interrupt") and subagent work folded into a single expandable row. To get there Xylo needs an ordered transcript model (`Vec<Segment>` where a segment is text, tool group, approval, or subagent) fed by event order, a parent id on child events, and a persisted copy of partial text.

### 5.2 Context window panel

Today: a ring showing summed prompt tokens, a dropdown with a chars/4 estimate that "excludes hidden system, tool and attachment context". The data for a Claude-style breakdown already exists in `GuardedProvider`: exact tool-schema tokens, system prompt tokens, per-message costs, the summary size, and the proactive/hard thresholds. Emit a `ContextUsage{system, tools, messages, summary, free, capacity, last_call_input}` event after each model call and render the breakdown; drop the sum.

### 5.3 Subagent viewer

Today: nothing. `agent_delegate` runs through Hermes' `SubAgentOrchestrator` (depth 1, two concurrent, 900 s timeout) and the child's `ToolStarted`/`ToolFinished` go to the same channel with no marker. Needed: `parent_action_id` on child events, a collapsible subagent row showing its task, elapsed time, tool count, and final summary (the real summary, not the 8 KB placeholder), plus a side panel listing live and finished delegates like Codex's agent list.

### 5.4 Model picker

Today: a popover with a "Model" title, reasoning dots, and a "model ›" button that opens a flat list of "model · provider name" plus "Manage models…". Adding a model means the provider editor, "Save & discover", then clicking a model, and discovery can swap the active provider as a side effect. Claude Code's `/model` is one list with the current model marked, grouped by provider, with the effort level inline and instant switch. Needed: grouped list with search, provider icons, context size and cost per entry, inline effort control, "last used" ordering, a separate "Test connection", a delete action, and no activation on discovery.

### 5.5 Permission modes

Today: four modes with three different label sets, "Applies to new tasks", tighten-only at admission, never re-read by a running turn, and a remember rule that collapses `bash -c …` to "all bash". Codex's control is two orthogonal settings (approval policy: untrusted / on-failure / on-request / never; sandbox: read-only / workspace-write / danger-full-access) with plain descriptions, applied to the next tool call. Claude Code re-reads the mode on every permission check (Shift-Tab switches live) and persists allow/deny patterns in settings. Needed: the broker reads the current mode from a shared cell the host updates (or the host forwards a `SetPermissions` command to the worker), the queue admission stops clamping, rules persist in settings with Claude-style patterns, one label set, and a screen to view and revoke remembered rules.

### 5.6 Conversation titles

Today: first 64 characters of the first prompt, fixed forever, no rename. Add an async title pass after the first Completed (small max-tokens call on the current provider), a `rename_conversation` host command with inline rename in the sidebar, and a "New chat · time" fallback.

### 5.7 Loading and working indicators

Today: `Status` events are rendered only when Hermes emits one; host statuses are dropped; admission (resource acquire, credential preflight, local inference start) emits nothing; the submission spinner has no timeout; Stop is hidden until `Started`. Needed: one persistent activity line (phase, elapsed, "esc to stop") driven by Started/Status/ToolStarted/model-call-started, host status surfaced as toasts, and a visible Stop from the moment Send is pressed.

### 5.8 Credentials and keychain prompts

Cause: unsigned or ad-hoc signed binaries change identity per build so "Always Allow" never sticks; the app and the worker each open the keyring; nothing caches. Fix in order: route every secret read through the host and cache in memory for the process; sign both binaries with one stable identity even for dev builds; add a pluggable `SecretStore` (OS keychain for signed releases; encrypted file with the master key as one keychain item on macOS or DPAPI on Windows; plain owner-only file as an explicit opt-in, which is what Codex uses). Windows should default to DPAPI.

---

## 6. Comparison with Claude Code and Codex

| Dimension | Xylo | Claude Code | Codex CLI | Net |
|-----------|------|-------------|-----------|-----|
| Process model | GPUI app + host thread + one worker process per chat over JSONL | One Node process, in-process tools | One process, `Session` per thread, SQ/EQ, app-server protocol | Xylo gains crash containment, loses cold-start per chat switch and in-process continuity |
| Transcript | SQLite messages + engine checkpoints; partials not persisted | Append-only JSONL per session, partials kept | Rollout JSONL | CLIs are greppable and replayable; Xylo has ACID admission and a tool journal |
| Agent loop | Vendored Hermes loop with Xylo callbacks; 200-call cap; loop guard → Error | Own loop, no cap | Own loop, no cap | Hermes brings identity/prompt pollution and nondeterministic repair |
| Edit tool | Exact search/replace + `replace_all`, diff preview; no read-before-edit enforcement, no write serialization | Same strategy, enforces Read first, serializes | `apply_patch` hunks | Xylo needs a per-file write lock |
| Approvals | 4 modes frozen per turn; session-memory rules | Modes re-read per call; persisted allow/deny patterns; hooks can decide | Approval policy × sandbox mode, per turn context | Xylo weakest on live switching and rule persistence |
| Sandbox | Anthropic sandbox runtime via Node helper, macOS/Linux, `.git` write-denied, MCP servers sandboxed too | Same runtime, native | Native seatbelt / Landlock | Xylo broadest coverage, worst failure UX (exit 125, missing Node) |
| Context | Exact tiktoken budget, model-written summary, in-memory per run, inline on hot path | Auto-compact + microcompact, persisted, `/context` breakdown | Auto-compact persisted, `/status` | Xylo has the best accounting and the worst visibility |
| Subagents | `agent_delegate`, depth 1, 2 concurrent, no UI | `Task` agents with definitions, parallel, visible | Agent list in TUI | Xylo needs a viewer and child markers |
| Hooks | Pre/PostToolUse only, each an approval, no `updatedInput` | 9 events, command/prompt/agent types, can modify input | Minimal | Xylo hooks unusable until re-classified |
| Plugins | Local/GitHub pinned, `.claude-plugin` and `.codex-plugin` import, WASM, grant review for native modules | Marketplaces, scopes | `.codex-plugin` | Xylo loses marketplace.json, settings.json, OAuth MCP |
| MCP | SQLite config, keyring secret, sandboxed stdio, per-turn spawn/kill, no OAuth, every tool a write | stdio/SSE/HTTP, OAuth, status UI | stdio/HTTP, OAuth | Xylo safer, slower, opaque failures |
| Memory | Scoped fact table, supersession, optional ONNX embeddings, learned procedures | CLAUDE.md files | AGENTS.md | Xylo not human-editable; degrades silently |
| Secrets | Keyring only, brokered to worker; but worker also opens keyring; header blocklist incomplete | Keychain / credentials file | `auth.json` 0600 | Xylo strictest in principle, prompts in practice |
| Chat UX | Queue button, steer pill, inline approval cards, DB search | Queue on Enter, Esc interrupt, slash commands, `@file`, plan mode, `/cost` | Same plus `/approvals`, `/diff` | Xylo lacks retry, stopped marker, slash commands, `@file`, plan mode, cost |

---

## 7. Recommended order of work

1. **Prompt and caching**: seed a Xylo `SOUL.md` or bypass `with_personality`; remove `with_timestamp` from the system block (put the date in the Xylo prompt only); gate every Hermes keyword hint and repo-review narrowing behind `native_product_finalization`. Small change, immediate quality and cost win.
2. **Durability**: switch the seven `conn.transaction()` sites and the learning persistence to `Immediate`; make `Store::call` wait instead of `try_send`.
3. **Edit race**: per-path mutex in `builtin.rs` around plan+write, and recompute the preview under the lock.
4. **Permissions**: broker reads a live mode; host forwards mode changes to the worker; drop the tighten-only clamp; persist rules; fix the `run:` rule to use the full argv; one label set; Codex-style descriptions.
5. **Hooks**: classify `extension_component` events as non-interactive (journaled, not approved) once the package is enabled; exempt from the read-only check.
6. **Transcript model**: ordered segments, parent ids on child events, persist partial text on cancel/error, settle unfinished cards, render host status, Stopped marker, Retry button, collapse finished tool groups, dedupe plan rows.
7. **Context panel and subagent viewer**: `ContextUsage` event from `GuardedProvider`; subagent rows and side panel.
8. **Model picker and provider lifecycle**: grouped searchable picker, no activation on discovery, delete provider, test connection, fix the dangling `credential_ref`.
9. **Host robustness**: Stop escalates to kill after a deadline; forward late Approve errors by session; don't hold `workers` while reaping; persist remembered rules; keep idle workers warm for N minutes instead of killing on switch.
10. **Credentials**: host-only keyring access with cache; stable signing identity; pluggable store with DPAPI/encrypted-file backends.
11. **Titles, quit flow, search**: async titles and rename; quit warns about running turns and offers "Quit anyway"; sidebar filter stays in the sidebar.
12. **Housekeeping**: purge tombstones (turn queue, guidance, fact links, capture ledger), map sandbox exit 125, MCP graceful shutdown and keep-alive across turns, header blocklist as an allowlist, dead code from `cargo check`, restore or remove the `docs/` links in the README.
