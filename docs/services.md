# Local services

`workbench-services` owns persistence, scoped memory, skill versions, native OS credentials, MCP sessions and five native service adapters. It is independent of the desktop toolkit and agent engine. No connector initializes and no embedding model loads merely because the app opens.

## Store and thread ownership

`Store::open(path)` returns a cloneable `Send + Sync` handle. A dedicated `workbench-sqlite` thread owns the connection. Its queue holds at most 128 jobs; a full queue returns an explicit busy error. The synchronous handle waits for that individual operation's result and must be called from the host worker, an I/O thread or `spawn_blocking`, never directly from an interactive rendering callback.

SQLite is bundled with FTS5, uses WAL, `synchronous=FULL`, foreign keys and a five-second busy timeout. Schema version 2 adds completion receipts without deleting version 1 history. Schema migrations reject a newer unknown database version. sqlite-vec is statically linked and registered once before opening connections; loading arbitrary extension paths is not enabled. The database contains references to credentials, never a credential vault.

The public Store API includes:

- `settings` / `save_settings`
- `conversations(limit)` / `upsert_conversation`
- `register_user_turn(&TurnRequest)`
- `messages(session_id, limit)` / `append_message`
- `save_checkpoint(session_id, turn_id, messages)` / `checkpoint(session_id)`, `completed_turn`, `complete_turn`
- `connectors` / `save_connector` / `delete_connector`
- `begin_action`, `mark_action_running`, `deny_action`, `finish_action`, `fail_action`, `mark_action_unknown`, `recover_interrupted`, `recover_session_interrupted`, `unresolved_actions`, `actions_by_name`, `reconcile_action`, `action(id)`, `action_for_key(session_id, turn_id, key)`
- `create_skill_version`, `evaluate_skill`, `promote_skill`, `rollback_skill`, `active_skill`, `skill_versions`, `skills`, `promoted_skills`

Conversation display history and opaque engine checkpoints are separate. Checkpoints preserve the original turn ID so resumed tools use the same durable call keys. Message and conversation pages are bounded to 1,000 items. The host creates the parent application-data directory before opening the database.

`register_user_turn` atomically inserts or touches the conversation and records the user message with UUIDv5(turn ID, `user-message`). Both host and worker can call it for the same request without duplicate history; later questions preserve the chat's original title/workspace. Reusing that turn ID with different text or a different session returns an error and commits no changes.

## Durable actions and recovery

An `ActionIntent` has an ID, session ID, turn ID, tool name, JSON arguments, write flag and stable idempotency key. The key is unique within session + turn. Reusing it for different arguments or a different tool is an error. `begin_action` returns `Execute`, `Reuse(Value)`, `InFlight` or `OutcomeUnknown`. A successful recorded result is reused without another external call.

Record `pending` before showing an approval. Denial transitions pending to denied. Only transition to running immediately before executing. Persist success and its provider result, or a confirmed failure. A disconnected or timed-out write transitions to outcome_unknown. The app must run `recover_interrupted` **once after its single-instance lock and before starting workers**, never while other workers are active. Recovery marks running writes unknown; unfinished reads and never-started pending intents become failed. Failed/denied keys cannot be replayed implicitly. An intentional new attempt needs a new key.

After a worker unexpectedly exits during a running application, `recover_session_interrupted(session_id)` applies those transitions only to that session, including its child tool namespaces. Call it only after confirming the process is reaped, and finish recovery before allowing a replacement worker to run. Other live sessions and successful/denied/previously uncertain results remain untouched. This operation is idempotent and never executes an action.

`reconcile_action` requires evidence: a confirmed external result can become successful and reusable; evidence of no effect becomes failed. Unknown operations cannot silently become executable. The journal does not claim external exactly-once semantics: providers and transports can lose responses after committing effects.

`messages_page(session_id, before, limit)` uses a bounded append-row cursor and returns chronological rows with a next-older cursor. `conversations_page(before, limit)` uses descending `(updated_at, id)` keyset pagination, preserving ties without offsets. Both clamp page size to 1..200 and fetch one extra row for `has_more`. Message cursors assume no `VACUUM` while a cursor is in use. Newer/updated conversations belong to a refreshed first page.

## Memory

`SqliteMemory::new(store)` is immediately usable for FTS5 lexical recall through synchronous methods or the asynchronous `workbench_core::MemoryBackend` interface. Scope is an exact string boundary supplied by the host; unrelated projects are never searched together. Supersession must reference a memory in the same scope, deactivates the old fact and commits the replacement atomically. Forget removes its text and vector entries.

Semantic retrieval is opt-in through `SqliteMemory::configured(store, data/embedding.json)`, used by both the engine and its tool broker. A missing file selects lexical retrieval. Existing malformed configuration returns a stable descriptive error. `reload_configuration(path)` validates a replacement before swapping the shared lazy session; invalid metadata preserves the previous configuration, and a missing file disables semantic mode. Call it on a blocking worker so active inference can finish without blocking the UI. Reading configuration is bounded to 64 KiB and validates metadata and file presence; it does not read model weights, load a native library, run inference or download assets. A direct `enable_embeddings(EmbeddingAssets)` API supports the same validation.

An example configuration is:

```json
{
  "directory": "embedding-assets/bge-small-en-v1.5",
  "model_id": "BAAI/bge-small-en-v1.5",
  "dimensions": 384,
  "cpu_threads": 0,
  "pooling": "auto"
}
```

Relative directories resolve against the configuration file; relative `runtime_library` paths resolve against the asset directory. The directory must contain compatible `model.onnx`, `tokenizer.json`, `config.json`, `special_tokens_map.json` and `tokenizer_config.json` files. It must also contain an ONNX Runtime library matching the application architecture and the pinned `ort` API: `libonnxruntime.dylib` on macOS, `onnxruntime.dll` on Windows or `libonnxruntime.so` on Linux. An optional `runtime_library` field selects another explicit path. Native dependencies required by that runtime must also be installed. No global loader environment variables are changed.

Pooling follows the model's training contract: `auto` chooses CLS for BAAI bge-small-en-v1.5 and mean for all-MiniLM; other models require explicit `cls` or `mean`. Declared dimensions must match the model output. Model, dimensions and pooling identify separate vector spaces, preventing mixed pooling indexes. Use a new model ID when replacing weights with another revision. FastEmbed is compiled without Hugging Face, model download or ONNX binary download features. No missing asset is downloaded automatically. Incompatible native libraries, tokenizer files or weights fail on first semantic use.

Initialization occurs on first semantic use, on a blocking worker, and one model mutex bounds inference concurrency. Zero CPU threads (also the default when omitted) uses current available parallelism, reserving one CPU for the UI when possible. A positive value is an upper bound clamped to current availability. `unload_if_idle(Duration)` drops an idle resident model while retaining metadata for lazy reload, and immediately returns false when inference is busy. The host can call it during idle/pressure maintenance. Embeddings use sqlite-vec `vec0` indexes with scope partition keys and cosine distance. Lexical and semantic candidate lists merge with reciprocal rank fusion. `reindex(scope, limit)` explicitly processes at most 1,000 unembedded/changed-model records. Switching models does not silently rebuild the whole store. Tests validate configuration laziness, pooling and real sqlite-vec retrieval without fetching models; real ONNX inference requires the user's compatible assets.

## Procedural learning and skill versions

The application now runs the full evidence pipeline described in [learning.md](learning.md): durable completed results or explicit user corrections create typed candidates, independent historical cases evaluate them, passing versions are promoted, scoped FTS retrieves them, and later matching results record reuse. Candidate extraction/evaluation runs off the foreground path. Arbitrary model-written instructions cannot promote themselves. Known verification commands remain subject to current ToolBroker approvals; external writes and uncertain outcomes cannot automatically qualify. Corrections retain source IDs, evaluation and append-only rollback history.

The underlying immutable skill-version Store primitives remain available, while the scoped LearningService enforces provenance and retrieval policy. Scores are historical outcome/template checks, not model-weight training or task-quality benchmark results.

## Credentials

`CredentialVault::new(service)` selects macOS Keychain, Windows Credential Manager or Linux Secret Service through keyring-core platform stores. Instance methods `get(reference)`, `set(reference, secret)` and `delete(reference)` serialize access. They return errors if the native store is unavailable or locked. There is no plaintext or environment-variable persistence fallback and no mutation of keyring-core's global default. Construction is lazy: the platform store opens on first credential operation, so an unavailable Linux desktop Secret Service does not prevent uncredentialed local file work. Native dialogs can block, so use the worker thread. Tests explicitly inject keyring-core's mock provider and never touch the user's real keychain.

## MCP

`ConnectorBroker::new(store, vault)` starts no sessions. Explicit `tools(config, cancel)` initializes a connector, negotiates MCP and discovers at most 512 tools / 2 MiB of schemas, caching them in SQLite. `cached_tools` reads metadata with no server startup. `call(config, tool, arguments, cancel)` executes through an existing or lazily created session. The host tool registry supplies tool search, selected schema loading, approval policy and action journaling.

A broker admits at most 32 active connectors, eight concurrent operations overall and four HTTP requests per connection. Sessions serialize application operations per connector; different connectors can progress independently. `disconnect` explicitly closes a session; `reap_idle` removes idle sessions. Local transport uses an existing absolute executable path and argument vector, never a shell string. An optional local credential is passed as `MCP_ACCESS_TOKEN`; a user-owned launcher can translate it to a provider-specific variable. Remote transport accepts HTTPS or loopback HTTP and disables redirects and session-recovery retries. All schemas default to write permission unless an MCP read-only annotation says otherwise; host policy may apply a stricter override.

Discovery has a 20-second initialization / 30-second catalog budget; calls have a 90-second deadline, cancellation and an 8 MiB result budget. A timeout or cancellation does not prove that a provider mutation was cancelled. Root action journaling must preserve uncertainty. Refreshing a saved credential requires disconnecting a warm MCP session before reusing it.

## Native APIs and OAuth

`NativeConnectors::tools(transport)` exposes four actual REST tools per provider. `NativeConnectors::call(config, name, arguments, cancel)` uses a pooled HTTP client, bounded pages and response size, explicit timeouts, and no automatic retry of business operations. Transport identifiers are:

| Transport | Implemented tools |
| --- | --- |
| `github` | Search issues, get issue, create issue, list pull requests |
| `google_drive` | List/search files, get metadata, export document text, create folder |
| `gmail` | Search messages, get full message, create RFC 5322 draft, send existing draft |
| `google_calendar` | List events, get event, create event, delete event |
| `slack` | Search messages, channel history, send message, update message |

Mutating definitions set `writes=true`; the application must obtain applicable approval before calling them. These are a useful explicit subset of each provider API, not a claim that every operation in those products is implemented. The configuration endpoint can be empty to use the official service origin, or explicitly set for a compatible enterprise API. Credentials are raw bearer tokens or JSON-encoded `OAuthTokens` stored in the OS vault. Provider HTTP errors and Slack's `ok:false` are returned as errors, never placeholder success.

`PendingOAuth::begin(OAuthConfig, redirect_uri)` creates a public-client PKCE S256 browser URL with random verifier/state, with no network work. The host owns an ephemeral loopback listener and opens that URL; the consuming `complete(code, returned_state, cancel)` validates state and expiry before exchanging the callback. The host stores returned OAuthTokens only in CredentialVault. OAuth app registration, scopes and provider consent remain provider requirements. `refresh_oauth` is explicit and rotates stored tokens; native calls do not refresh and replay a write after an authentication error. Providers requiring confidential-client secrets or nonstandard auth need a backend/adapter extension; secrets must not be bundled into the renderer.

## Validation boundaries

The crate's tests cover SQLite reopening, checkpoint/history separation, FULL durability, duplicate result reuse, argument-key collisions, denied calls, interrupted writes and reconciliation, scope-safe memory replacement/forget, FTS punctuation, real sqlite-vec KNN partitioning, skill evaluation/promotion/rollback, injected credential behavior, PKCE state rejection, real HTTP error/route handling and an actual rmcp initialization/discovery/call exchange against a local protocol fixture. Fixtures make no external account changes. Production provider OAuth/permissions, OS dialogs on each release platform and real ONNX inference still need integration acceptance with the user's configured accounts/assets.

## Worker engine integration and completed-turn receipts

The engine restores interrupted tool calls using the checkpoint's original turn ID before accepting a new turn. Successful results come from the journal; failed/denied calls become error observations rather than being redispatched. Unknown effects block continuation. Child agents use stable parent-call namespaces in journal keys, an independent registry without delegation, and a dispatch guard that also rejects delegation through `call_tool`. Their provider and tool permissions remain inherited from the parent.

The engine records the full successful tool result before attempting artifact/UI presentation. If artifact storage fails, the user sees an explicit presentation error while the confirmed result remains in the journal. Error status observations and nonzero process exits are not presented as successful tool outcomes. Reused large results are passed through the same presentation boundary.

`complete_turn(session, turn, messages, text, usage)` commits the final engine checkpoint, one assistant display message and a durable completion receipt in one SQLite transaction. The engine checks this receipt before model initialization. If the worker or host loses the Completed acknowledgement, retrying the same session + turn returns the saved text/usage without another model call or duplicate history. Cancelled and unfinished turns save partial checkpoints without a completed receipt.

Provider requests have a context preflight and output reserve. Recognized OpenAI-compatible model names use the bundled tiktoken model mapping and embedded BPE assets; unrecognized/custom models use conservative UTF-8 byte accounting, not a `characters/4` estimate. Wire framing and tool schemas are included. This is a guard, not an exact tokenizer for arbitrary GGUF models, and can reject a custom-model request that might otherwise fit. Overflow is an explicit recoverable configuration error before contacting the provider. No Ollama-specific `num_ctx` field is added to generic OpenAI requests.

A generation admission wrapper can hold a cross-process file lease only while a request/stream is active; stream completion, drop, timeout and cancellation release it. It never holds a generation lease across tool execution. External Ollama/vLLM endpoints retain server-managed concurrency. Only an application-managed loopback endpoint explicitly listed in `data/managed-endpoints.json` with `single_slot: true` uses this lease. Provider requests and helper tasks also acquire automatic live CPU/RAM demand leases; helper parallelism follows current headroom rather than fixed RAM tiers.

The desktop adapter enables `native_product_finalization`; its upstream-compatible default is false. Product mode accepts a natural final answer after the actual tool loop finishes, without additional provider calls demanding engineering evidence formatting or research report boilerplate. Tool execution, approvals, cancellation, context checks and durable completion checks remain active. Detailed engine status stays in tracing logs; progress events expose short product text. Streaming and nonstreaming regression tests run a real file-read tool followed by a natural answer and assert exactly two model calls.

Native model provider configuration, account sign-in and protocol fixture coverage are documented in [providers.md](providers.md).
