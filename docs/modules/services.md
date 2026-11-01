# Durable local services (`xylo-services`)

## Purpose and boundaries

`xylo-services` is the crate of durable local application services: the SQLite database, secret storage, memory, history search, action journal and review, composer and document drafts, a durable turn queue and steering, provider discovery and sign-in, MCP and native connectors, portable extension packages (skills, plugins, marketplace directory), a sandbox, image generation, hooks and settings files. Its crate docs (`crates/services/src/lib.rs`) state: "Durable local application services. Blocking APIs must be called off the UI thread. No service contacts a provider or loads embedding assets during construction."

Dependencies (`crates/services/Cargo.toml`): `xylo-core` is the only Xylo crate (contracts live in [core](core.md)); storage uses `rusqlite` (bundled, `hooks` feature) and `sqlite-vec` (pinned `=0.1.9`); embeddings use `fastembed` and `ort` with dynamic loading; secrets use `keyring-core` with per-OS stores; MCP uses `rmcp`; plus `reqwest` (rustls), `chacha20poly1305`, `hmac`, `sha2`, `zeroize`, `jsonwebtoken`, `fs2`, `zip`, `serde-saphyr` (YAML), `image` and others. Feature `test-support` is empty and exists for downstream test code. Orchestration that uses these services lives in [runtime](runtime.md) and the engine.

## Module map

All paths are under `crates/services/src`. `lib.rs` re-exports most modules with `pub use`; `action_review`, `attachments`, `extensions`, `image_generation`, `providers`, `sandbox`, `settings_files` and `hooks` are public modules.

| Path | Purpose |
| --- | --- |
| `schema.rs` | The single database schema, `user_version` check and reset (`DatabaseReset`) |
| `store.rs` | `Store`: dedicated SQLite thread; settings, conversations, messages, completed turns, action journal, connectors |
| `checkpoint.rs` | Engine checkpoint rows and `CheckpointBase` |
| `action_review.rs`, `action_review/` | Review, resolution and leases for journaled actions; per-session review forms |
| `credentials.rs`, `credentials/` | Secret storage (OS keychain or encrypted local file) |
| `memory.rs`, `memory/redaction.rs` | Memory store with FTS5 and optional semantic search |
| `memory_records.rs`, `memory_facts.rs`, `memory_forms.rs` | Memory record helpers, versioned facts, memory forms |
| `history_search.rs` | Full-text search over conversation titles and messages |
| `learning/` | Learning jobs, cases and candidate skills |
| `drafts.rs`, `document_drafts.rs`, `document_drafts/` | Composer drafts and document drafts with relinking |
| `turn_queue.rs`, `steering.rs` | Durable turn queue and mid-turn guidance |
| `usage.rs` | Per-turn token usage records |
| `attachments.rs` | Image attachment storage |
| `providers/` | Model discovery, served context, ChatGPT sign-in, AWS signing |
| `connectors/` | MCP, native connectors, OAuth, IDE and project MCP |
| `extensions/`, `extension_receipts.rs` | Portable extension packages: manifest, catalog, sources, marketplace directory, execution runtime |
| `sandbox.rs` | Process sandbox helpers |
| `image_generation.rs`, `image_generation/http.rs` | Image generation requests |
| `settings_files.rs`, `hooks.rs`, `instruction_sources.rs` | Settings files, hooks, instruction source records |
| `tests/`, `*/tests*` | Unit tests |

## Database (`schema.rs`, `store.rs`)

**There are no migrations.** The schema (`SCHEMA`, `VERSION = 2` in `PRAGMA user_version`) is created whole in a new file. A database is accepted only when `user_version` is `2` and its schema objects equal those of `SCHEMA` (tables named `memory_vec_*` are excluded because semantic memory adds them at run time). Otherwise, including a file that is not a database, the owner closes and **deletes the database with its `-wal` and `-shm` files and creates an empty one**; old data is never read.

- `Store::open(path)` is the owner mode (`OpenMode::Owner`): sets `journal_mode=WAL`, may create or reset. Call it from exactly one process (the host); it does not create parent folders. `Store::reset_notice()` returns a `DatabaseReset` whose `Display` reads "The local database was replaced with a new, empty one because …. Previous chats, settings and history were deleted."
- `Store::open_existing(path)` (`OpenMode::Existing`, for workers and tools) validates the same way but **never creates, migrates, renames or deletes**: a missing file, empty database or other schema is an error ("Only the application host may create or reset it").
- Every connection sets `busy_timeout` 5 s, `synchronous=FULL`, `foreign_keys=ON` and checks `vec_version()` so `sqlite-vec` is loaded.
- `Store` is a cloneable handle to one thread named `xylo-sqlite` that owns the connection and runs boxed jobs in order. At most `QUEUE_CAPACITY = 128` operations are admitted; further callers wait up to `QUEUE_WAIT = 30 s`, then fail with "The database is busy; retry later". A stopped thread yields "Database worker unavailable".
- Read-then-write transactions use `write_transaction` (`TransactionBehavior::Immediate`), so the busy timeout applies rather than failing at lock upgrade.
- Page limits: `MAX_PAGE = 1000` for `messages` and `actions_by_name`; `messages_page` clamps to 1..=200 and pages by rowid in append order (the database must not be `VACUUM`ed while those cursors are in use).

Tables (all in `SCHEMA`): `settings`, `native_execution_grants`, `conversations`, `messages`, `checkpoints`, `checkpoint_messages`, `completed_turns`, `turn_usage`, `instruction_sources`, `actions` and the action review tables, `extension_*` tables, `connectors` and connector catalog tables, `memories` (+ `memories_fts`, `memory_vector_tables`, fact tables, `memory_forms`), `skills`, `skill_promotions`, `learning_*`, `composer_drafts`, `turn_queue`, `user_guidance`, `document_drafts`, `document_draft_relinks`, `history_search_documents` (+ `history_search_fts`). Several tables have size-check constraints and usage-counter triggers (see Data and persistence).

## Conversations, journal and checkpoints (`store.rs`, `checkpoint.rs`)

**Conversations and messages.** `Conversation` and `ChatMessage` are stored as JSON (`conversations.json`, `messages.json`). `register_user_turn` creates the conversation if absent (title is the first 64 characters of the prompt, or "Image conversation" for an empty prompt) and inserts the user message with id `Uuid::new_v5(turn_id, "user-message")`. It rejects more than `MAX_ATTACHMENTS_PER_TURN` images ("Attach at most four images"), images of another conversation, and a reused turn id with different input. Also present: `rename_conversation`, `set_conversation_pinned`, `set_conversation_archived`, `set_conversation_workspace`, `conversations`, `conversations_page` (keyset), `conversation_workspaces`, `worktree_conversations`.

**Completing a turn.** `complete_turn` (and `_as`, `_with_reasoning`) runs **one immediate transaction** that writes the checkpoint rows, the assistant message (id `assistant_reply_id(turn_id)`), the `completed_turns` receipt, removes the interrupted-reply placeholder and the turn's `turn_usage` rows, touches the conversation and queues a `learning_jobs` row. Replaying with an identical result is a read-only no-op; a different result fails with "Turn was already completed with a different result". `record_interrupted_reply` stores a non-`Complete` partial reply (ignored if blank or the turn already completed).

**Action journal.** `begin_action(ActionIntent)` requires a non-blank `idempotency_key`, scoped to session and turn, and returns an `ActionDecision`:
- new key: inserts `pending`, returns `Execute`;
- `succeeded`: `Reuse(result)`; `pending`/`running`: `InFlight`; `outcome_unknown`: `OutcomeUnknown`;
- `denied` and `failed` return errors ("choose a new attempt key after reviewing the failure"); reuse with different name, arguments or `writes` is rejected.

State transitions (`mark_action_running`, `finish_action`, `fail_action`, `mark_action_unknown`, `deny_action`) are compare-and-set on the previous state and fail with "Action is absent or has an incompatible state". `recover_interrupted` (run once after taking the single-instance lock) marks `running` write actions `outcome_unknown`, other `pending`/`running` actions `failed`, and `running` extension receipts `outcome_unknown`. `recover_session_interrupted` does the same for one confirmed-dead worker's session only. `reconcile_action` requires non-blank evidence: `None` result means confirmed no effect, `Some` means confirmed success.

**Checkpoints.** One header row (`checkpoints`: turn, length, revision, `fallback_model`) plus one row per message (`checkpoint_messages`). A write rewrites only from the first changed message. `save_checkpoint_messages(..., base: Option<CheckpointBase>)` uses the writer's last revision and unchanged prefix; a stale base falls back to comparing stored rows, and **stored rows always equal the last messages written**. A short stored row set fails with "Checkpoint messages are incomplete". `select_checkpoint_fallback` works only on the current turn's checkpoint header (a stale writer cannot select a model for a newer turn) and rejects a blank model.

## Secrets vault (`credentials.rs`, `credentials/`)

`CredentialVault` is an encrypted vault for provider keys, tokens and connector secrets. Settings only hold credential references (see [core](core.md)); values live here.

- **One file**, `<data>/secrets.vault`, encrypted as a whole with XChaCha20-Poly1305 (`chacha20poly1305`). Layout (`credentials/file.rs`): 8-byte header `XYLOVLT1` (also the AEAD associated data), a 24-byte random nonce per write, then the ciphertext of a JSON map of references to secrets. **A wrong key, truncation or tampering fails authentication and returns an error, never garbage.** Files over `MAX_VAULT_BYTES` (16 MiB) are refused.
- The 32-byte `VaultKey` is zeroed on drop, parsed only from exactly 32 bytes ("Vault key has the wrong length") and is never logged, put in settings, arguments or environment variables. The host passes it to a worker over a private stdin pipe; `CredentialVault::unlocked` builds a worker vault that performs no I/O at construction, never opens the keychain or key file and never deletes anything (an unreadable vault is an error).
- Key location (`credentials/key.rs`): `SecretStorage::LocalFile` uses `<data>/secrets.key` (owner-only permissions); `SecretStorage::OsKeychain` uses one keychain item with account `vault-key` (service `CREDENTIAL_SERVICE`). The choice is recorded in `<data>/secrets.storage`; **that record, not the settings database, decides where an existing vault's key is read**, so a reset database never strands secrets. `CredentialVault::storage()` reports the storage actually in use.
- `CredentialVault::open(data_dir, storage)` (host only) loads the key and reads the vault at once. A vault that can never be read (wrong or corrupt key, missing key beside an existing vault, damaged file) is **deleted and replaced by an empty vault with a new key**; `reset_notice()` returns the one-time message. With no record both places are tried before deleting. Transient failures (locked keychain, I/O error) are returned as errors and delete nothing.
- `change_storage` and `start_over` discard saved secrets: a new key is created and read back in the new place first, then the vault is deleted, the record updated and the old key removed. A failure before the switch leaves the old key in use.
- Concurrency: entries are cached per instance (shared by clones) and reloaded when the file changes (identified by a `FileStamp`). Writes take an exclusive `fs2` lock on `<data>/secrets.vault.lock`, re-read, and replace the file atomically by rename, so host and workers do not lose each other's entries. Files are created owner-only (`private_options`, `set_private`) and the directory is synced after rename.
- All operations may block (file locks, keychain dialogs): **call off the UI thread.**

## Memory (`memory.rs`, `memory/redaction.rs`, `memory_facts.rs`, `memory_records.rs`, `memory_forms.rs`)

`Memory` wraps a `Store` and implements `xylo_core::MemoryBackend`. Recall is hybrid: FTS5 BM25 lexical results (always available) fused by reciprocal rank with optional semantic results.

**Writes** (`remember_sync`) are non-destructive and provenance-bound:
- Content and source are first passed through `redact_secrets`. Then the record needs a non-blank scope (at most 8192 bytes, no NUL), non-blank content of at most 262,144 bytes, source at most 8192 bytes; otherwise "Memory requires scope and bounded content". A record cannot supersede itself.
- Saving an identical record again is a no-op that does not load the embedding runtime; reusing an id with different content is an error. A version in the deleted-ids table is rejected ("This memory version was deleted; refresh before saving a new entry").
- A correction (`supersedes`) must target an active record in the same scope and **may come only from the same or a more trusted `MemoryOrigin` tier** ("replacing it needs a correction the user approves"). The replaced version is deactivated, not deleted. A scope generation check fails with "Project facts changed during this save; refresh and retry".
- If embedding fails the fact is still saved for lexical recall.

**Recall** (`recall_sync`, `recall_with_history`): empty query or `limit == 0` returns nothing; `limit` is capped at 100; at most 32 whitespace-separated terms are quoted and joined with `OR`; each ranker contributes `1/(60+rank)`, ties broken by newer `created_at`. Only active records of the scope are returned. A semantic failure logs a warning and recall continues lexically. `forget_sync` and `forget_scoped` delete only the selected version; other versions are not reactivated. `reindex(scope, limit)` embeds missing vectors outside the transaction and skips memories forgotten or superseded meanwhile.

**Embeddings.** Configured from explicit local assets (`EmbeddingAssets`; model, tokenizer, config files and an ONNX Runtime library; pooling must be explicit `cls` or `mean`). Construction validates metadata and paths only; **loading is lazy and never downloads**. `Memory::configured` treats missing configuration as lexical mode and an invalid existing configuration as an error. After a failure, load and inference are not retried for `EMBEDDING_RETRY_AFTER` (10 minutes) unless configuration changes; `EmbeddingStatus` reports not configured, configured or failed, and `take_embedding_failure` returns the newest failure once. Vectors live in per-model `memory_vec_*` tables listed in `memory_vector_tables`. `reload_configuration` validates before swapping; `unload_if_idle` never interrupts a busy inference.

**Redaction** (`memory/redaction.rs`) replaces with `[REDACTED]` private-key blocks, values with known provider prefixes (body at least 16 characters, so `sk-learn` survives), JSON Web Tokens, `Bearer`/`Basic` credentials, passwords in URLs, mysql-client `-p` passwords and values of credential-named keys. It is idempotent.

**Fact versions** (`memory_facts.rs`): `fact_versions`, `fact_delete_preview` and `delete_facts` inspect and delete version chains; they never touch chat, checkpoints, learned procedures, composer drafts or unsaved forms. Deletion needs a preview whose scope revision and counts still match ("Project facts changed; review the deletion again"), is idempotent by `operation_id` (a reused id with a different request fails), is cancellable, removes FTS and vector rows, re-parents surviving versions, remembers deleted ids for 30 days (`DELETED_RETENTION_MS`) and keeps the newest 4096 receipts.

**Browsing and export** (`memory_records.rs`): `memory_page` uses keyset paging (`limit` clamped to 1..=100). `export_memories` and `export_fact_versions` stream to a destination in a separate read transaction at one snapshot, are cancellable and omit embeddings.

**Memory forms** (`memory_forms.rs`): unsaved Memory form state with revisions and `mutation_id` for conflict detection (`MemoryFormSave::Saved` or `Conflict`). Limits: 6 MiB per form (`MAX_FORM_BYTES`), 64 MiB total (`MAX_TOTAL_BYTES`), 4096 rows; form text at most 262,144 bytes. A form is never a fact until the user saves it.

## History search (`history_search.rs`)

The index is part of the schema: `history_search_documents` plus the external-content FTS5 table `history_search_fts` (`unicode61`), kept current by triggers on `messages` and `conversations`, so **there is no separate indexing phase** and a write is searchable atomically. Message rows use document id `rowid*2`, conversation titles `rowid*2+1`.

- `search_history(query, before, limit)`: the query is at most 512 bytes ("History search query exceeds 512 bytes"); `limit` is clamped to 1..=200; results are the best-ranked document per matching conversation, paged by `(updated_at, id)` (a `ConversationCursor`), not by a floating-point rank.
- `history_window` centers a page on a source message that must belong to the session; `history_after(session, after, limit)` pages forward in append order and requires `after > 0` ("Invalid history append cursor"). Cursors are rowids and ignore timestamps.

## Hooks (`hooks.rs`)

Command hooks run lifecycle scripts and return validated control output.

- `EVENTS`: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SubagentStop`, `SessionStart`, `SessionEnd`, `PreCompact`, `Notification`, `PermissionRequest`. `configured` / `configured_alias` read `hooks.<event>` from an `EffectiveSettings` snapshot (see `settings_files.rs`); an unknown event is an error and `disableAllHooks: true` yields none.
- Each group has an optional regular-expression `matcher` (empty or `*` matches all) tested against the tool name and a Claude alias (`shell_command`/`run_command` as `Bash`, `file_read` as `Read`, `file_edit` as `Edit`, `file_write` as `Write`, `web_fetch` as `WebFetch`, `find_files`/`files_list` as `Glob`, `search_project` as `Grep`). Only `type: "command"` hooks are supported; a command must be non-blank and at most 64 KiB; `timeout` defaults to 30 s and is clamped to 1..=300 s; at most 128 hooks per event.
- `run` executes `/bin/sh -c <command>` through `sandbox::prepare_coding` **offline in the project sandbox**, with `.xylo` and `.claude` under the workspace added to protected paths. Input JSON is limited to 1 MiB and output to 1 MiB; stderr is discarded. Exit code `2` becomes `{"decision":"deny"}`; any other non-zero exit is an error; empty output is `{}`; non-JSON output is an error. Cancellation or timeout reports that effects may be incomplete.
- `HookOutput::parse` accepts Claude's `hookSpecificOutput` form and the flat form: `permissionDecision`/`decision` (`allow`/`approve`, `deny`, `ask`, `block`), `updatedInput` (object), `additionalContext` (string), `continue: false` with `stopReason`. Unknown decisions are errors. **Arbitrary output never gains execution authority.** `merge` ranks decisions `Deny` > `Ask` > `Allow`, so a later allow never erases an earlier denial.

## Settings files and permission rules (`settings_files.rs`, `instruction_sources.rs`)

Layered settings and source-labelled permission rules, **read fresh at each boundary** so edits and revocations apply to the next action. Invalid files fail closed (an error naming the file); credentials stay in the vault.

- `SettingsFiles::for_workspace` lists, in order: a managed file (macOS `/Library/Application Support/Xylo/managed-settings.json`, Windows `%ProgramData%\Xylo\managed-settings.json`, otherwise `/etc/xylo/managed-settings.json`), then `~/.claude/settings.json`, `~/.claude/settings.local.json`, `~/.xylo/settings.json`, then in the workspace `.claude/settings.json`, `.xylo/settings.json`, `.claude/settings.local.json`, `.xylo/settings.local.json`. From any path under `.claude` **only `permissions` and `hooks` are read**. Files are limited to 1 MiB. `from_paths` takes explicit sources for isolated tests.
- `EffectiveSettings { value, rules }` keeps each `PermissionRule { pattern, decision, source, managed }`. `evaluate(name, arguments)` returns `Deny` before `Ask` before `Allow`, regardless of file order; a `path` argument is resolved against the canonical workspace before matching. Patterns are non-blank and at most 8192 bytes and must end with `)` when they have an argument part. `disableAllHooks` must be a boolean; keys disabled by the managed file are forced to `true`.
- `save_rule(pattern, decision, project)` atomically writes `.xylo/settings.json` (project) or `.xylo/settings.local.json`, and requires the directory to remain inside the project; `remove_rule` removes a rule.
- `instruction_sources.rs`: `record_instruction_sources` stores canonical instruction files (no content) per conversation before their text is emitted; `session_instruction_origin` returns the least-trusted `MemoryOrigin` ever exposed; `copy_instruction_provenance` must run before a fork's first turn (failure must abort the fork). **Exposure is conservative: rewinding does not erase it.**

## Composer drafts, document drafts, turn queue and steering (`drafts.rs`, `document_drafts.rs`, `turn_queue.rs`, `steering.rs`)

All are durable, revisioned and idempotent by a caller-supplied mutation id, with expected-revision conflict detection; each has a size-checked table with usage triggers (`composer_draft_usage`, `document_draft_usage`).

- **Composer drafts** (`composer_drafts`, key session and workspace): text at most 262,144 bytes (table `CHECK`); attachments stored as JSON. Admission of a draft as a turn shares `register_user_turn_in_transaction` with `register_user_turn`, atomically.
- **Document drafts** (`document_drafts`, key workspace and source): JSON at most 6,291,456 bytes and valid JSON (table `CHECK`). `document_drafts/relink.rs` re-attaches a draft to a moved source; its receipts (`document_draft_relinks`) are **immutable** (triggers abort update and delete).
- **Turn queue** (`turn_queue`): FIFO by `sequence` per session, states `queued`, `removed`, `started`, a `paused` flag, tombstones for removed entries and a unique `(session_id, workspace, revision)`.
- **Steering** (`user_guidance`): durable mid-turn guidance with states `queued`, `applied`, `cancelled`, unique per `(session, turn, workspace, draft_revision)`.

Limits (constants in the source):

| Item | Limit |
| --- | --- |
| Composer draft text / attachments | `MAX_DRAFT_BYTES` 256 KiB; `MAX_DRAFT_ATTACHMENTS` 4; image references JSON at most 8192 bytes |
| Composer drafts total | `MAX_DRAFT_STORAGE_BYTES` 64 MiB; `MAX_DRAFT_ROWS` 16,384 |
| Document drafts | `MAX_DOCUMENT_DRAFT_BYTES` 6 MiB each; 64 MiB total; 4096 rows; 4096 edits; export name at most 480 bytes |
| Queued turns | 8 per chat ("This chat already has eight queued messages"), 64 globally; tombstones kept 7 days, at most 16,384 |
| Steering | 64 items and 128 KiB of text per turn; 16,384 queued overall; finished guidance kept 30 days |

## Usage and attachments (`usage.rs`, `attachments.rs`)

**Usage.** `record_turn_usage(session, turn, call, usage)` replaces one request's latest token receipt in `turn_usage` (keyed by call id) and is skipped once the turn has completed; `complete_turn*` deletes the turn's rows after the final usage is stored in the receipt. `turn_usage` and `usage_report` return per-message `Usage`, conversation `UsageTotals` and local-day totals (turns, input, output, cache read and write tokens, `estimated_usd`, `unpriced_calls`). Queries run on the store thread.

**Attachments.** `AttachmentStore` is "native, bounded image import"; call it off UI threads. `import_file`, `import_clipboard` and `import_bytes` accept PNG, JPEG, WebP or GIF ("Choose a PNG, JPEG, WebP or GIF image"), bound input by `MAX_IMAGE_BYTES` (from core) and decoding by `MAX_PIXELS` 40,000,000, `MAX_DIMENSION` 16384 and `MAX_DECODED_BYTES` 192 MiB, under a `.decode.lock`; decoding is cancellable. Each import writes `original`, `provider` (re-encoded, at most `MAX_PROVIDER_IMAGE_BYTES` 3 MiB), `preview.png` (256 px, at most 512 KiB) and `receipt.json` into a hidden `.<id>.partial` staging directory that is removed on drop, then renames it to `<root>/<session_id>/<attachment_id>`. **Only explicit user imports accept outside paths; durable references never do.** `validate` and `read_image` check the session; `read_provider_image` and `preview_path` serve the derived variants.

## Providers (`providers/`)

- **Endpoint normalization** (`providers/mod.rs`): `normalize_provider(&ProviderConfig)` accepts a service root, version root or full inference endpoint and returns inference, stream and models URLs per `ApiFamily`. **It requires HTTPS, or HTTP only for loopback hosts** (`localhost`, `127.0.0.1`, `[::1]`: "Use HTTPS or a loopback HTTP provider endpoint"); queries are preserved, credentials and fragments in the URL are rejected. It validates custom headers before a connection or secret is persisted. `protocol_capabilities(family)` describes wire-protocol flags only.
- **Credentials**: `ProviderCredential` (`None`, `ApiKey`, `Bearer`, and others) intentionally has no `Debug` or `Serialize` for secret-bearing variants; `ProviderCredential::headers` builds request headers.
- **Custom headers** (`validate_custom_headers`): at most 16 headers, names at most 128 and values at most 4096 bytes; transport headers (`host`, `content-length`, `connection`, `transfer-encoding`, and so on) and `proxy-*` are refused; **any name containing `auth`, `token`, `key`, `secret`, `cookie`, `session`, `signature`, `password`, `credential` or `bearer`, or starting with `x-amz-`, is refused** because custom headers live in plain settings. Credentials belong in the vault.
- **Model discovery** (`discover_provider_models`): read-only, bounded to eight pages and 2,000 models, never starts a model process and never invents choices. The credential is read from the vault with `spawn_blocking`; a missing configured credential fails with "Provider credential is not configured". Context sizes of local servers are lowered to the served value.
- **Served context** (`providers/served_context.rs`): asks a **loopback** server for the context it actually serves (llama.cpp `GET /props`, Ollama `GET /api/ps` then `POST /api/show`, LM Studio `GET /api/v0/models`) without a proxy or credentials. `PROBE_DEADLINE` is 2.5 s, `REQUEST_TIMEOUT` 1 s, at most 32 shown models, minimum `MIN_SERVED_TOKENS` 256. Any failure means unknown and the configured window stays; probing never loads a model. `ServedContextCache` remembers answers per owner.
- **Sign in with ChatGPT** (`providers/chatgpt.rs`, `providers/sign_in_service.rs`): the public-client flow against `https://auth.openai.com` for resource `https://api.openai.com/v1`. `ChatGptCredential` is kept whole in the vault and deliberately has no `Debug`. `ChatGptSignInService::new(data_dir)` creates `<data>/auth-locks` and admits at most 8 concurrent operations; cross-process locks serialize rotating refresh tokens. The loopback callback server accepts at most 16 simultaneous connections and 32 invalid requests, only from loopback peers, and the whole sign-in times out after 600 s or on cancellation. `MAX_ACCESS_TOKEN_LIFETIME_SECS` is 30 days. Also: `refresh_chatgpt_credential`, `revoke_chatgpt_credential`, `sign_out`, `account`, `resolve_provider_credential`, `refresh_rejected_provider_credential`.
- **AWS** (`providers/aws_signing.rs`): SigV4 only for the two fixed read-only Bedrock catalog endpoints; no credential process, shell or environment mutation.

## Connectors and MCP (`connectors/`)

Crate docs: "Lazy MCP sessions and native API adapters. Policy and durable action journaling belong to the application tool registry; all native writes are labelled as such." Connector records are stored in the `connectors` table (`save_connector`, `delete_connector`, `connectors` on `Store`).

**`ConnectorBroker`** (`connectors/mcp.rs`) keeps one MCP connection per connector, reused while the configuration and local launch scope are unchanged; clones share the pool. Constants: `MCP_IDLE_TIMEOUT` 300 s (a background reaper, `REAP_INTERVAL` 30 s, closes idle connections and lives only as long as the pool), `INITIALIZE_TIMEOUT` 20 s, `DISCOVERY_TIMEOUT` 30 s, `CALL_TIMEOUT` 90 s, `CLOSE_GRACE` 5 s, `CANCEL_NOTICE_TIMEOUT` 1 s, **at most 8 concurrent calls** (`Semaphore::new(8)`), and the first 20 lines / 4096 bytes of a local server's stderr are kept for connection errors. Closing is graceful: an in-flight request is withdrawn with `notifications/cancelled`, the client is closed, and only then is the sandboxed process group killed (`LocalProcess` also kills it on drop).
- Transports: stdio child processes (sandboxed, scoped by `ConnectorLaunchScope`: workspace, read roots, protected paths, allowed domains and an explicit environment, **never the inherited parent environment**), streamable HTTP through `BoundedHttpClient` (every body capped by `HTTP_BODY_LIMIT` 8 MiB so a hostile server cannot exhaust memory) and a legacy HTTP+SSE transport restricted to a same-origin POST endpoint (`mcp/legacy.rs`).
- `validate_endpoint` rejects URLs with embedded credentials and allows only HTTPS or loopback HTTP ("Use HTTPS or loopback HTTP for a connector").
- `call_reviewed` re-checks a `ConnectorCatalogAuthority` (id, trust flag, fingerprint of config and scope) and the current catalog snapshot, failing with "MCP configuration or scope changed after review; review the call again" or "MCP catalog changed after review; discover and review the call again". Arguments must be a JSON object of at most 1 MiB. Cancellation is honoured while waiting for capacity or the session.
- Catalog generations (`mcp/catalog.rs`, tables `connector_catalog_state`, `connector_tools`) fence stale discovery results and survive restart; if persisting fails the broker refuses calls until storage is repaired. `mcp/metadata.rs` handles deferred resources and prompts with bounded pagination (`connector_prompts`). `mcp/handler.rs` routes server notifications and **declineable** elicitation forms to the active chat (`answer_form`).
- `abort`, `disconnect`, `reap_idle`, `shutdown`, `configuration_guard` (per-connector mutex) manage lifecycle.

**Project MCP** (`connectors/project.rs`): `project_connectors(workspace)` reads `<workspace>/.mcp.json` inertly: no process, network or credential store is touched. The file must resolve inside the workspace, be at most 256 KiB and define at most 32 servers; names are 1..=128 characters with no CR, LF or NUL. Connector ids derive from a SHA-256 of workspace and name. **A project server runs only after `approve_project_mcp`** (stored in `project_mcp_consents` by fingerprint; `project_mcp_approved` checks it).

**IDE bridge** (`connectors/ide.rs`): private, project-scoped discovery of records (at most 16 KiB each, at most 32 bridges); the token stays in memory and never enters saved connector config.

**Native connectors** (`connectors/native.rs`): `NativeConnectors::tools(transport)` defines tool sets for `github`, `google_drive`, `gmail`, `google_calendar` and `slack`; mutating tools set `writes`. One pooled `reqwest` client: connect timeout 10 s, request timeout 60 s, **redirects disabled, retries disabled**, at most 8 concurrent requests. `validate_arguments` checks arguments against the tool schema. `ConnectorNotDispatched` marks a definitive local rejection before any HTTP future ran (message cut to 2048 characters, error chains dropped so credential-store details do not leak).

**OAuth** (`connectors/oauth.rs`, `oauth/discovery.rs`): authorization-code with PKCE (`PendingOAuth::begin`/`complete`), `refresh_oauth` and `connector_access_token`. Refreshes are **single-flight per credential reference** in-process: a waiter re-reads the stored record and skips its exchange if another caller already rotated the token. `OAuthTokens` is deliberately not `Debug`. `discover_oauth` performs protected-resource and authorization-server discovery with native public-client registration.

## Extensions: portable packages, skills and plugins (`extensions/`, `extension_receipts.rs`)

Crate docs: "Portable extension packages. Discovery never executes package code. Installed resources are immutable snapshots; readiness is per component." `ExtensionService::new(root)` keeps a `catalog.json` and an exclusive `catalog.lock` (`fs2`) under its root. Catalog or runtime-setup files that no longer decode are **deleted, not migrated**, so their packages read as not installed and setups as not configured.

**Components and readiness** (`extensions/manifest.rs`): `ComponentKind` is `Skill`, `Command`, `Agent`, `Hooks`, `Mcp`, `LanguageServer`, `HostedIntegration`; `Readiness` is `Installed`, `NeedsSetup`, `UnsupportedRequirement`, `ExecutionVerified`. `ExtensionPackage` records id, name, version, source, revision, content `digest`, `enabled` and components. Metadata YAML (`serde-saphyr`) is limited to 256 KiB, instruction files to 256 KiB, a package to 1000 components, and an inspected package must contain at least one component ("No portable extension components found").

**Install** (`extensions/catalog.rs`): `install_local` and `install_discovered` copy the source into a staging directory under the root, then snapshot it. The copy refuses symlinks and non-regular files and enforces depth at most 32, at most 10,000 files and **64 MiB** total. The package id is `extension-` plus the first 24 hex characters of SHA-256 of the provenance string; the content digest is a tree hash. Reinstalling changed content changes the digest, which invalidates earlier setup and approvals. `set_enabled`, `remove`, `list` manage the catalog; `package_root` and `read_resource` stay inside the package.

**Skills.** `skill_metadata()` lists enabled in-scope skill, command and agent components. `load_skill(id, arguments, automatic)` requires a namespaced `<package>:<name>` id, arguments of at most 64 KiB, an enabled package, no `UnsupportedRequirement`, honours `disable-model-invocation` for automatic loads and `user-invocable` for explicit ones, rejects the reserved control marker `U+001F` in text, and allows at most 16 dynamic context commands. `extensions/discovery.rs` copies project and user skill sources into the same reviewed snapshot catalog.

**Sources and directory.** `ExtensionSource { name, repository, revision, subdirectory }` names a GitHub repository (`from_input` validates the repository form); `default_sources()` includes `openai/plugins` and `anthropics/claude-plugins-official`; `save_sources` allows at most 32. `install_discovered` downloads a pinned archive (`zip`), rejecting symlinks, more than 10,000 files, over 64 MiB, and an archive whose entry length does not match. Network use is explicit and never happens while rendering the UI or constructing the service. `extensions/marketplace.rs` builds a cached directory per source under `<extensions>/directory/` from `.claude-plugin/marketplace.json` or `.agents/plugins/marketplace.json` (falling back to the tree layout): `directory(query)` answers instantly from cache; `refresh_directory(force)` revalidates with `If-None-Match` and a `DIRECTORY_TTL_SECONDS` of 6 hours. Limits: index 4 MiB, manifest 256 KiB, tree 8 MiB, 2000 entries, 8 concurrent manifest fetches. On a GitHub rate limit the cached directory is kept with the status "Showing cached results; GitHub rate limit reached".

**Execution** (`extensions/runtime.rs`): "Import and discovery never spawn commands or look up credentials." `ComponentSetup` (workspace, non-secret environment, vault `credential_refs`, `allowed_domains`, `enabled_events`, `timeout_seconds`) is explicit per `<package>:<component>`; runtime setup files are at most 1 MiB and at most 128 executable entries. `execute_component_bound` ties an approval to the exact immutable package and reviewed setup ("Extension changed; review and configure its new version", "Component changed since approval; review its setup again") and checks cancellation before dispatch. Hooks run only for events enabled in the setup (`matching_hooks`, `run_hooks`; input at most 1 MiB); LSP components use Content-Length framing (headers at most 8 KiB, response at most 4 MiB, request at most 1 MiB); MCP components use `ConnectorBroker` with `MCP_CLOSE_GRACE` 10 s. The caller must authorize and journal mutations before invoking execution.

**Receipts** (`extension_receipts.rs`): settings-triggered executions get durable receipts in `extension_execution_receipts` without creating fake chats; `extension_action_bindings` record an immutable binding per action ("Extension action binding cannot change"). `recover_interrupted` marks `running` receipts `outcome_unknown`.

## Procedural learning (`learning/`)

Crate docs: "Scoped, evidence-grounded procedural memory. No model weights are trained and no learned document executes tools or changes permissions." `LearningService::new(store)`.

- **Jobs.** Completing a turn (and recovery) inserts a `learning_jobs` row per `(session, turn)`. `LearningService::run(cancel)` loops until cancelled: one backfill page of completed turns (32) plus up to four jobs per pass (`process_pending(4, ..)`), sleeping `IDLE_DELAY` 2 s when idle. A failed pass logs a warning and retries with growing delay up to `MAX_FAILURE_DELAY` 60 s, so **a failure never ends background learning**; durable jobs stay queued. A job is claimed with an owner and lease (`state` `pending` to `processing` to `done` or `failed`); finishing requires the same owner ("Learning job lease changed"). Evidence processing retries `PROCESS_ATTEMPTS` 3 times with `PROCESS_BACKOFF` 250 ms.
- **Evidence.** Cases are built from journal actions of the turn that `succeeded` with a result (results over 262,144 bytes are not read), at most `MAX_STEPS` 16 steps and `MAX_CASES` 16 cases per comparison. Text that looks sensitive (`sanitize.rs`: `authorization`, `bearer `, `api_key`, `password`, `secret`, `-----begin`, `ghp_`, `github_pat_`, and others) is excluded.
- **Candidates and promotion.** Skills are versioned rows in `skills` with state in `learning_candidates` (`candidate`, `promoted`, `suspended`, `superseded`); promotion supersedes the previous promoted version and is recorded in `skill_promotions`; `rollback(scope, name, version)` restores a version; a failing evaluation suspends. Evaluation is the historical template `historical-outcome-template/v1`. Retrieval is FTS5 over `learning_fts`, per scope; `record_retrieval` stores outcomes (`learning_retrievals`) shown by `reuses`.
- **Corrections.** `record_correction` / `record_scoped_correction` take a `CorrectionInput`; **the guidance must occur verbatim in the persisted user message, never a tool result**, must be inside the selected workspace scope, and a replacement must be in the same scope.
- `status(scope)` returns `LearningStatus` (pending and failed jobs, candidates, promoted, verified reuses); `review` records human review evidence.

## Sandbox (`sandbox.rs`)

"Managed OS sandbox launcher. Missing dependencies fail closed; the caller owns the returned process group and cancellation for its whole lifetime."

- The sandbox is bootstrapped by a Node runtime that itself runs unsandboxed, so it must be trusted: the bundled runtime next to the helper, then `XYLO_SANDBOX_NODE` (`NODE_OVERRIDE`, not subject to ownership checks), then a `node` found on `PATH` (or Homebrew on macOS). A discovered binary is accepted only when it and every directory above it (up to but not including `HOME`) is owned by the current user or root and not world-writable or writable by a non-root-equivalent group; the minimum is Node 22.12 (`MINIMUM_NODE`). When none qualifies, `SandboxUnavailable` names each rejected binary, why and how to fix it (message prefix `XYLO_SANDBOX_UNAVAILABLE:`).
- Launcher exit codes: `UNAVAILABLE_EXIT` 125 (setup failed) and `SPAWN_FAILED_EXIT` 127. **Start failures and blocked connections are never reported on the program's stdout or stderr**, which the program controls: each `PreparedSandbox` has a private status file (`status_file`) the sandboxed program can neither read nor write; read it with `read_start_status` (a `StartFailure` means nothing ran) and `read_network_denials`. The status file is limited to `MAX_STATUS_BYTES` 256 KiB.
- `prepare`, `prepare_with_policy`, `prepare_coding`, `prepare_coding_with_network`, `prepare_coding_with_scope` build commands. `Network::None` (default) blocks all outbound connections; `Network::Unrestricted` allows any host through the runtime's proxy, which still refuses loopback, link-local and cloud-metadata addresses. Coding commands may use installed host Rust binaries but never the user's Cargo config or credentials; cache and home writes stay in an owned temporary root.
- `set_child_input` limits input to 1 MiB ("Sandbox input exceeds 1 MiB"); `set_child_environment`, `ensure_writable_target`, `trusted_paths`, `read_guard_exit_code`, and `wait_for_guard_cleanup` (failures are `CleanupUnknown`).

## Image generation (`image_generation.rs`, `image_generation/http.rs`)

"Explicit native image generation. A submitted request is never replayed automatically: transport loss and cancellation keep a durable uncertain result." `ImageGenerationService::new(data_dir, artifacts)` creates `<data>/image-generation` (job files with the request SHA-256 and receipt) and `<artifacts>/generated`.

- `validate_provider`: name non-blank and at most 160 bytes, model chosen explicitly (at most 256 bytes), `timeout_seconds` 1..=600, endpoint at most 8192 bytes, configuration at most 32 KiB, credential reference 1..=256 bytes, custom headers pass `validate_custom_headers`, and the endpoint passes `normalize_provider` (HTTPS or loopback HTTP).
- Generated images are decoded from base64 or fetched, bounded by `MAX_IMAGE_BYTES` (8 MiB: "Generated image is empty or exceeds 8 MiB"); `read_bounded` caps response size and is cancellable. A request index holds at most 64 ids. Cancellation during save fails with "Image save cancelled".
- `generate` returns an `ImageGenerationReceipt`; `receipt` and `recent_receipts` read stored receipts; `save_provider` and `configured_provider` persist the chosen provider.

## Action review (`action_review.rs`, `action_review/`)

"Operator-only review of uncertain effects. **Observation never authorizes replay.**" It handles journal actions left `outcome_unknown` (see Conversations, journal and checkpoints).

- `action_reviews` pages actions for an `ActionReviewScope { session_id, workspace }` using a snapshot sequence from `action_review_clock` (triggers on `actions` bump it); a stale cursor fails with "Refresh the action list". `action_review` returns an `ActionReviewDetail` with `BoundedActionPayload`s.
- `resolve_action(ActionResolutionRequest, cancel)` records an operator's observation, `ActionResolutionOutcome::Completed` or `DidNotComplete`, with required evidence. It is idempotent by `operation_id` (reuse with different observations fails: "This review identity was reused with different observations"), requires the expected revision to match and the action to still be `outcome_unknown` ("This action changed; refresh it before recording an outcome"), and checks cancellation before commit. Receipts live in `action_resolutions` and are **immutable** (triggers abort update and delete); the original action JSON and its SHA-256 are kept.
- Review forms (`action_review/forms.rs`, table `action_review_forms`, at most 16,384 bytes each, at most 512 forms) keep unsaved recovery input per session with revision checks.
- Leases (`action_review/leases.rs`): `ActionExecutionLease` is a kernel-owned `fs2` file lock striped over 4096 lock files under `<database>.action-leases` (directory mode `0700`, files `0600`, `O_NOFOLLOW`; a symlinked or foreign-owned directory is refused). A new execution waits up to `EXECUTION_LEASE_WAIT` 10 s for a busy stripe; review and recovery test the lock once because a held stripe means "possibly still running" (`action_review_busy`, `action_review_status`).

## Data and persistence

| Store | Location | Notes |
| --- | --- | --- |
| Application database | the path given to `Store::open` | SQLite, WAL, `synchronous=FULL`, schema version 2, reset on mismatch |
| Action leases | `<database>.action-leases/` | 4096 striped lock files, owner-only |
| Secret vault | `<data>/secrets.vault`, `secrets.vault.lock`, `secrets.key`, `secrets.storage` | Encrypted; key file or OS keychain item `vault-key` |
| ChatGPT sign-in locks | `<data>/auth-locks/` | Cross-process refresh serialization |
| Image attachments | `<artifacts>/<session_id>/<attachment_id>/` | `original`, `provider`, `preview.png`, `receipt.json` |
| Generated images | `<data>/image-generation/`, `<artifacts>/generated/` | Job files and artifacts |
| Extensions | the `ExtensionService` root: `catalog.json`, `catalog.lock`, immutable package snapshots, `directory/` cache | Undecodable catalog or setup files are deleted |
| Settings files | managed, user and workspace JSON files (see Settings files) | Read fresh, never cached across actions |

Rules: the database is created and reset only by the owning host (`Store::open`); workers use `Store::open_existing`. Message, conversation, action, memory and queue payloads are JSON columns, so formats are defined by the `xylo-core` types. There is no migration path for the database, vault or extension catalog: incompatible data is discarded, with a one-time notice for the database (`DatabaseReset`) and the vault (`reset_notice`). Size and count limits are enforced twice where they protect storage: in code and by table `CHECK` constraints with trigger-maintained usage counters (composer drafts, document drafts, memory forms, review forms).

## Concurrency and error behavior

- **Blocking APIs must run off the UI thread.** `Store` methods block the caller until the `xylo-sqlite` thread finishes (and up to 30 s waiting for a queue slot), vault operations take file locks and may show keychain dialogs, and `AttachmentStore`, `ExtensionService` and settings-file functions do synchronous file I/O. Async APIs (`discover_provider_models`, `ConnectorBroker`, `NativeConnectors`, sign-in, image generation, `hooks::run`, `LearningService::run`) take a `CancellationToken` where they can wait; where they need the vault or database they use `spawn_blocking`.
- **No construction-time side effects:** constructing services never contacts a provider or loads embedding assets; extension install, discovery and import never execute package code.
- **One writer thread.** All database work is serialized on one connection. Read-then-write operations use immediate transactions so the 5 s busy timeout applies across processes (host, workers, tools).
- **Idempotency and replays.** Turn completion, memory saves, fact deletions, action resolutions, relinks and draft writes are keyed by ids; an identical replay returns the stored result, and the same id with different content fails.
- **Failure is explicit and fail-closed.** Invalid settings files, undecodable vault files, unexpected schemas, stale revisions and changed configurations return errors naming the cause; the exceptions that discard data (database, vault, extension catalog) are documented above and always surface a notice or "not installed" state. An interrupted side effect is never replayed: it becomes `outcome_unknown` until an operator records evidence.
- **Optional features degrade, not fail:** a failing embedding runtime falls back to lexical memory recall; an unreachable served-context probe keeps the configured window; a rate-limited marketplace refresh keeps the cached directory; a failed learning pass retries later.
- Errors are `anyhow::Result`; typed markers exist where callers must classify: `SandboxUnavailable`, `CleanupUnknown`, `ConnectorNotDispatched`.

## Security and privacy

- **Secrets never live in settings, turns, checkpoints or arguments.** Settings hold references; values are in the encrypted vault (XChaCha20-Poly1305, authenticated, owner-only files, zeroed keys). `ProviderCredential`, `ChatGptCredential`, `OAuthTokens` have no `Debug` for secret fields.
- **Redaction before persistence:** memory content and source are redacted of credentials before saving; learning excludes sensitive text; custom provider headers that look like credentials are refused.
- **Transport:** provider, connector and image endpoints require HTTPS or loopback HTTP and reject embedded credentials; native connectors disable redirects and retries; remote MCP bodies are capped at 8 MiB.
- **Local execution is sandboxed and explicit:** hooks run offline in the project sandbox; MCP stdio servers get an explicit environment and a sandboxed process group; project `.mcp.json` servers need per-fingerprint approval; extension components run only after explicit, version-bound setup; project files cannot grant themselves permission (`.claude` settings files contribute only `permissions` and `hooks`; managed settings can force-disable keys).
- **Provenance:** memory origin is assigned by the application and corrections need the same or higher trust; instruction exposure per conversation is conservative and survives rewind.
- **Audit integrity:** action resolutions and document relink receipts are immutable by trigger.

## Testing

Tests are unit tests inside the crate (`src/tests/mod.rs` for store behavior and `tests` modules or files next to `action_review`, `attachments`, `checkpoint`, `connectors`, `credentials`, `document_drafts`, `drafts`, `extensions`, `history_search`, `image_generation`, `learning`, `memory`, `memory_facts`, `memory_forms`, `memory_records`, `providers`, `sandbox`, `settings_files`, `steering`, `turn_queue` and `usage`). Most open a temporary `Store` via `tempfile`; `CredentialVault::for_tests` creates a private temporary vault. Run (not run while writing this document):

```
cargo test --locked -p xylo-services
cargo test --locked -p xylo-services <filter>
```

The `test-support` feature is enabled by `xylo-engine` for its tests.

## Related crates

- [core](core.md): shared contracts (`Conversation`, `ChatMessage`, `MemoryRecord`, `ProviderConfig`, `ConnectorConfig`, `Usage`, `MemoryBackend`) that this crate stores and implements.
- [runtime](runtime.md): host-side orchestration; it depends on `xylo-services` (see its `Cargo.toml`).
- [engine](engine.md): the turn engine; depends on `xylo-services` (with `test-support` in tests).
- Other direct dependents (from `Cargo.toml`): `cli`, `evals`, `worker`.
