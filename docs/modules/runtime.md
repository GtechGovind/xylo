# Native application runtime (`xylo-runtime`)

## Purpose and boundaries

`xylo-runtime` is the native application layer between the UI/CLI frontends and the lower-level crates. Its crate doc is "Native application services." It owns the event-driven **application host** (`host`), worker process supervision, bounded process execution, the PTY terminal, language-server sessions, the project catalogue and Git inspection, update orchestration, observability and several thin off-UI wrappers over the stores in [services](services.md).

Boundaries, from `crates/runtime/Cargo.toml` and the sources:

- Depends on `xylo-core` ([core](core.md)), `xylo-services` ([services](services.md)), `xylo-resources`, `xylo-scheduler`, `xylo-helper-client`, `xylo-distribution` and `xylo-notifications`. It re-exports `xylo_scheduler` as `scheduler` and `xylo_notifications` as `notifications`, and re-exports a set of document-draft, memory-form, fact, usage and steering types (`crates/runtime/src/lib.rs`).
- **The engine depends on runtime, not the reverse.** `crates/runtime/src/headless.rs` states that a headless frontend constructs its engine after opening the profile, so runtime never links `xylo-engine`. The host starts the agent as a separate `xylo-worker` process and a `xylo-helper` process, located as siblings of the current executable (`native_executable` in `host.rs`; a missing sibling fails with "A required application component is missing. Reinstall the complete application.").
- It contains no UI toolkit code. Events leave through bounded channels (`HostEvent`), and the UI sends bounded commands through `HostClient`.
- Platform-specific dependencies: `portable-pty` and `alacritty_terminal` (terminal), `notify` (file watching), `ignore` plus `grep-*` (project search), `rusqlite` (the private workspace catalogue), `fs2` (profile and activity locks), `libc` (Unix process groups) and `windows-sys` (Windows only).

Two small crates under `crates/runtime/validation/` (`terminal`, `windows-workspace`) are standalone validation packages, not part of the library. The terminal one includes `src/terminal.rs` by `#[path]` so its tests can run against the real implementation.

## Module map

| Path under `crates/runtime/src` | Purpose |
| --- | --- |
| `lib.rs` | Module declarations, re-exports, `AppPaths` |
| `host.rs` | `HostClient`, `HostEvent`, `HostCommand` and the host command loop (`HostState`) |
| `host/worker_control.rs` | Worker spawn, handshake, event reader, warm idle pool, stop escalation, control-reply routing |
| `host/turn_queue_control.rs` | Per-session queued next-turn intents; the single Stop implementation |
| `host/steering_control.rs` | Durable mid-turn guidance |
| `host/interaction_control.rs` | Wakeup admission (loops) and answers to questions and plans |
| `host/credential_control.rs` | Active-turn authorization of worker credential renewal |
| `host/provider_control.rs`, `web_search_control.rs`, `image_control.rs` | Provider discovery, web-search settings and image operations that touch secrets |
| `host/settings_control.rs` | Ordering of settings commits and project opens |
| `host/fact_control.rs`, `document_control.rs`, `usage_control.rs`, `export_control.rs`, `git_review_control.rs`, `mcp_control.rs`, `notice_control.rs`, `update_control.rs`, `worktree_control.rs` | Host commands for Memory, document drafts, usage/status line, exports, Git hunks, MCP sign-in, notifications, updates and worktrees |
| `headless.rs` | `Profile`: exclusive profile ownership for the CLI |
| `process.rs` | Bounded process execution (plain and sandboxed) |
| `terminal.rs` | PTY session and Alacritty-based grid snapshots |
| `lsp/` | Language-server sessions for the editor (`mod.rs`, `service.rs`, `client.rs`, `transport.rs`) and for agent tools (`agent.rs`) |
| `workspace/` | Project catalogue (`index.rs`, `monitor.rs`, `rules.rs`, `paths.rs`), confined reads (`reader*.rs`), search, read-only Git (`git.rs`, `git/hunks.rs`), owned worktrees, output styles |
| `updates.rs` | Check, download and install lifecycle over `xylo_distribution::Updater` |
| `observability.rs` | Daily/size rolling private logs and opt-in OTLP export |
| `status_line.rs` | Sandboxed user-configured status command |
| `drafts.rs`, `steering.rs`, `turn_queue.rs`, `history.rs`, `learning.rs`, `extensions.rs`, `conversation_export.rs` | Off-UI wrappers and re-exports over `xylo_services` stores |
| `tests/`, `*/tests.rs`, `*_tests.rs` | Unit and integration-style tests |

## Key types and public API

### `AppPaths` (`lib.rs`)

`AppPaths { data, database, artifacts, models }`. `AppPaths::discover()` uses `directories::ProjectDirs::from("com", "qurkos", "xylo")` and its `data_local_dir()`, failing with "Could not locate the Xylo application directory". `AppPaths::at(data)` sets `database = data/xylo.db`, `artifacts = data/artifacts`, `models = data/models` and creates `data`, `artifacts` and `models`. Tests pass explicit temporary directories.

### `HostClient` and `HostEvent` (`host.rs`)

`HostClient::spawn()` / `spawn_at(AppPaths)` start the host and return `(HostClient, Receiver<HostEvent>)`. The module doc: UI methods enqueue bounded commands and return immediately; database, credentials, process I/O and networking stay off the UI.

- Commands go through an `async_channel` bounded at **128**. Every `HostClient` method uses `try_send` and fails with "Application host queue is full or disconnected" instead of blocking.
- Events go through an `async_channel` bounded at `xylo_core::EVENT_QUEUE_CAPACITY` (256).
- `HostEvent` is a large enum covering settings, conversations and history pages, worker `EngineEvent`s wrapped with per-session host sequence numbers, permissions, provider catalogs (`ModelsFailed`), workspace indexing, LSP responses and diagnostics, Git results, memory, connectors, schedules, usage, updates, worktrees (`ChatWorktree`, `Worktrees`) and lifecycle (`Status`, `Error`, `Stopped`).
- `HostClient` methods are grouped by area: conversations (`list_conversations_page`, `rename_conversation`, `set_conversation_pinned`, `set_conversation_archived`, `search_history`, `history_window`, `history_after`, `load_session_page`, `export_conversation`), drafts (`load_draft`, `save_draft`, `list_drafts`, `submit_draft`), settings and providers (`save_settings`, `save_provider`, `save_provider_connection`, `remove_provider`, `discover_models`, `cancel_model_discovery`, `save_web_search`), workspace (`open_workspace`, `refresh_workspace`, `workspace_files`, `search_project`, `cancel_project_search`, `open_file`, `save_file`, `git_status`, `git_file_diff`), editor (`lsp_request`, `lsp_change`, `lsp_close`, `lsp_cancel`), worker control (`send`, `send_to`), memory, connectors, schedules (`create_schedule`, `list_schedules`, ...), `capability`/`cancel_capability`, worktrees and `shutdown`.
- `foreground(active)` records GPUI visibility only; no model or tool can synthesize it.

### Startup sequence (`HostClient::spawn_at`)

On a thread named `xylo-host` the host:

1. Takes an exclusive `try_lock_exclusive` on `data/host.lock`; a second instance fails with "Xylo is already running for this profile".
2. Builds a multi-thread Tokio runtime with **2** worker threads.
3. Opens `Store` at `xylo.db`, then runs `recover_steering` and `recover_turn_queue` for saved drafts and queued turns. Action tracking is transient and is not recovered.
4. Loads settings with `startup_settings`: if stored settings no longer decode, **defaults are saved** and a notice ("Your saved settings could not be read, so Xylo started with default settings.") is emitted. There is no compatibility parsing of older formats.
5. Opens the secret vault (`open_secrets`) and aligns `settings.secret_storage` to the storage the vault actually records (`follow_vault_storage`). If the vault cannot be opened, the host keeps running with the error as state and emits "Saved keys are unavailable: ...".
6. Creates the ChatGPT sign-in service, notifications service, updates service, semantic memory (`SqliteMemory::configured` with `data/embedding.json`; on failure it falls back to `SqliteMemory::new` and emits "Semantic search needs setup in Downloads. Your saved memories remain searchable."), the workspace service (cache in `data/workspace-index`) and the LSP service.
7. Starts notification tasks, the background `LearningService::run`, forwarders for workspace and LSP events and the schedule loop, then processes commands until `HostCommand::Shutdown`.

If the vault is switched to a different storage while unreadable, `reopen_secrets` starts a new vault without the saved keys and the host shows "Secret storage changed. Saved keys were removed; reconnect your providers."

### Command ordering and admission (`host.rs`, `host/settings_control.rs`)

- `SaveSettings`, `SaveWebSearch`, `SaveProvider` and `RemoveProvider` reserve a commit ticket **at receipt**, before any task can race for admission; their commits run in receipt order. Dropping a queued ticket forwards completion only after its predecessor, so cancellation cannot let a later snapshot overtake an in-progress commit.
- Project opens (`Workspace`) are ordered among themselves, not behind settings commits, so a slow provider save cannot block them. Editor (`Lsp`) commands wait for every project open received before them.
- `settings_control` merges the live project fields (`last_workspace`, `ui.open_projects`) into a settings snapshot when a project open received after the snapshot has already been applied (`latest_project_open`).
- While an update install is in progress (`installing`), `Workspace` and `Lsp` commands are dropped, and LSP requests get "Wait for the update installation to finish".
- `state.capture_stop(command)` captures Stop synchronously at intake. It is documented as the only cancel path and applies to a Stop without a target only when exactly one chat is running or starting (`host/turn_queue_control.rs`).
- A shared `operations` read/write gate keeps workers from starting while an update installs (`host/update_control.rs`, `host/turn_queue_control.rs`).
- `effective_mode(mode, scheduled)`: **scheduled turns run unattended, so `FullAccess` becomes `ApproveForMe`**; every other mode, and every interactive or queued turn, is unchanged (`host/worker_control.rs`). Settings are the source of truth for the permission mode and changes are forwarded to live workers (`forward_permissions`).
- Local-model setup serializes through a lock file: `acquire_model_activity` takes a shared lock, retries every 20 ms and fails with "Local model setup is busy; try again shortly" after **15 s**.

### Worker supervision (`host/worker_control.rs`)

Each chat gets a `WorkerHandle` for a `xylo-worker` child process (stdin/stdout/stderr piped, `kill_on_drop`). The command channel to the worker holds 64 envelopes (`mpsc::channel`).

- **Handshake invariant:** `Unlock` (the vault key, exported on a blocking thread) is the first command, then `Hello`. A worker is used only after it answers `Hello` with `Ready` in this host's protocol version, within `HANDSHAKE_TIMEOUT` = **20 s**; otherwise the process is killed and spawn fails ("The task runner did not respond during startup").
- Child `RUST_LOG` is inherited, else `debug` when debug tracing is enabled, else `info`. Worker stderr is logged through `redact_secrets`.
- Events of the active run reach the UI with per-session host sequence numbers. Run state is cleared only after its terminal event is in the UI channel.
- Replies to control commands (`Approve`, `SetPermissions`, `Cancel`, ...) are routed by request id through a ledger of at most `CONTROL_LEDGER_CAPACITY` = 128 entries rather than dropped. A decision for a run that is gone is reported as "The task is no longer running, so this decision was not applied."
- **Stop escalation:** `stop_active_run` sends `Cancel` once per run request. If it cannot be delivered, or the run reaches no terminal event within `STOP_GRACE` (**10 s**, 300 ms in tests), the process is killed; the reaper records interrupted actions and reports a cancellation ("The task did not stop in time, so Xylo ended it.").
- **Warm pool:** idle workers stay warm for `IDLE_WORKER_TTL` = **5 minutes**, at most `MAX_IDLE_WORKERS` = **4**, least recently used retired first. `retire_workers_for_new_key` retires workers when the vault key changes.
- Credential renewal for an active turn (`host/credential_control.rs`, `WorkerReader::renew_credentials`) is bounded by a semaphore of 4 concurrent renewals; only the active turn may request one.

The host exposes no manual action-review commands or events. Worker retirement ends the process and settles unfinished turns and queues without reconciling an action journal. Live tool cards use worker events. Private `LiveActivity` updates populate the host’s transient `ActiveAction` cache after matching the active request, session and turn; they never reach the UI event stream. The worker drains updates before terminal events, and a final marker enables complete imported evidence for learning. Lag, oversized payloads and worker crashes mark evidence incomplete. Command output logs and native command operation status remain separate.

### Other host controls (`host/*_control.rs`)

Each file extends `HostState` for one concern; the module docs state the guarantees:

- `turn_queue_control`: per-session ordered queue of next-turn intents. A queued intent does not touch the current worker; only durable admission may move it into the transcript. When a run ends the next queued message starts, or the queue pauses with a reason shown on each item ("You stopped the task. Resume the queue when ready.", "The task ended with an error. ...", "Paused while an update installs. ...").
- `steering_control`: durable mid-turn guidance. Transport is reserved before saving guidance; only the engine's durable application receipt may clear a captured draft. The worker never supplies draft content; the host rereads its saved guidance.
- `interaction_control`: conversation-owned wakeup admission and explicit loop controls; explicit mode controls and plan answers obey unattended authority.
- `credential_control`: private worker credential renewal authorized only for the active turn.
- `web_search_control`: saves the search credential before publishing non-secret references. SearXNG endpoints must be HTTPS with no credentials, query or fragment. Only this module can construct `AuthorizedSearchSettings`, the authority to replace vault-owned settings.
- `image_control`: validates metadata, saves the secret in the vault, then publishes the connection; the UI receives only the credential reference. Receipts preserve uncertain outcomes rather than retrying an external write.
- `provider_control`: model discovery with a generation counter shared with `HostClient`, so a newer save invalidates older catalogs at enqueue time, before any vault I/O.
- `fact_control`: Memory operations; SQLite, embedding and exports never run on UI or async event threads, and accepted form saves are awaited through their commit.
- `document_control`: document-edit recovery; conversion stays in the supervised helper and these commands only use the database executor.
- `usage_control`: usage and the optional status command stay off the UI thread.
- `mcp_control`: user-initiated MCP browser sign-in with a bounded, one-use loopback callback.
- `notice_control`: only live worker envelopes with a matching transport request produce lifecycle notices; history replay never does.
- `update_control`: install is a separate explicit action; work must finish first and no update retries an uncertain write or silently cancels a task. It requires `updates.status()["can_install"] == true`, refuses queued or network work, then takes the exclusive `operations` gate.
- `worktree_control`: `create_chat_worktree` is allowed only for a new chat (no worker, no queued run, no stored conversation) and creates `chat-<session id>` from `HEAD`; startup cleanup of archived worktrees; manual removal. Shutdown timeout is 10 s (300 ms in tests).
- `git_review_control`: applies one explicitly selected hunk from an unchanged diff preview, outside model tool execution.
- `export_control`: conversation export for a native save dialog (see `conversation_export` below).

## Process execution (`process.rs`)

Shared by Git, search, shells and diagnostics.

- `run(program, args, cwd, timeout, cancel)` executes argv directly: **no shell is inserted and arguments are never interpolated**. `run_sandboxed`, `run_prepared`, `run_prepared_sandboxed`, `run_prepared_sandboxed_input` and `run_prepared_observed` cover prepared commands.
- Output is drained continuously (8 KiB reads) so a child cannot deadlock on a full pipe; **only the first 512 KiB per stream is retained** (`OUTPUT_LIMIT`) and `ProcessResult.truncated` reports the loss. An `OutputObserver` callback sees every chunk.
- Each unsandboxed child runs in its own process group on Unix (`process_group(0)`); the group is `SIGKILL`ed on cancellation, timeout or drop of the future. On Windows the child is created with `CREATE_NO_WINDOW`.
- A timeout returns "Process timed out; side effects may have occurred". Callers must preserve outcome-unknown for interrupted side effects. Child reaping waits at most 5 s and captured output at most 2 s after termination.
- `NotStarted` marks a command that cannot have had any side effect: cancelled before start, spawn failure, or sandbox start failure. For sandboxed commands this is read **only from the launcher's private status file**, never from the program's output. `CleanupUnknown` is re-exported from `xylo_services::sandbox`.
- Guarded sandbox runs set `XYLO_SANDBOX_PARENT_GUARD=1`; the sandbox helper's watchdog, not this crate, kills the group if the owner exits. Input to a sandboxed child is limited to 32 KiB.
- `require_success` turns a non-success exit into an error containing the code and stderr.

## Terminal (`terminal.rs`)

`TerminalSession::spawn(cwd)` opens a PTY (initially 24 rows by 80 columns) with `portable-pty`, starts `$SHELL` (fallback `/bin/sh`; on Windows `COMSPEC`, fallback `powershell.exe`) with `TERM=xterm-256color` and `COLORTERM=truecolor`, and returns the session plus a `tokio::sync::watch::Receiver<TerminalSnapshot>`. The doc says to call it from a background executor because spawning a shell can block. All process, I/O, parser and search work runs on dedicated threads (for example `terminal-supervisor`, which owns and drops the child); the UI receives **coalesced immutable snapshots** at a `FRAME_INTERVAL` of 16 ms. Input commands travel over a `sync_channel` of 64. Controls: `input`, `paste` (optionally bracketed; `paste_bytes`), `resize`, `scroll`, `select`, `search`. Parsing uses `alacritty_terminal`. Teardown is covered by lifecycle tests in `terminal/coalescing_tests.rs` and `terminal/tests.rs`, and by `validation/terminal`.

## Language servers (`lsp/`)

Application-owned sessions: servers start on first editor use, share a bounded native transport and are released after their documents close. **Project files never select executable commands**; overrides live in app data.

- Server resolution (`resolve_server`): an optional `data/language-servers.json` (at most 1 MiB; `deny_unknown_fields`; at most 16 servers) maps languages to an **absolute** executable path (canonicalized, must be a file) with at most 64 arguments totalling at most 32,768 bytes. Otherwise built-in defaults are looked up on `PATH`: `rust-analyzer` (build scripts, proc macros and check-on-save disabled) for Rust and `clangd` (`--background-index=false`, `--header-insertion=never`, `--limit-results=200`) for C-family languages. Other languages fail with "Install a native language server for <language> in Settings to enable code intelligence". A `rustup` shim is never launched.
- Limits: `MAX_SERVERS` 16 (one per workspace and language; "Close an unused project before starting another language server"), `MAX_DOCUMENTS` 64, `MAX_TEXT` 4 MiB per document, `IDLE` 300 s (`lsp/mod.rs`); `lsp/client.rs` also defines a `CAPACITY` of 64 for its bounded channels.
- Only UTF-16 position encoding is supported; a server that negotiates another encoding is shut down with an error.
- `lsp/transport.rs`: Content-Length framing with `MAX_FRAME` 8 MiB and `MAX_HEADER` 8192 bytes; sizes count UTF-8 bytes and an invalid header is rejected before any body allocation.
- `lsp/service.rs` orders editor synchronization (monotonic document versions handle out-of-order host scheduling) and runs read requests cancellably and concurrently. `LspEvent` is `Diagnostics` or `Response`.
- `lsp/agent.rs` (`AgentService`) gives agent tools awaitable language intelligence over the same native transport.
- The host rejects LSP requests with "Open a project to use code intelligence" when no project is open.

## Rolling diagnostic logs

Logs live under the application data directory's `logs/` folder (macOS: `~/Library/Application Support/com.qurkos.xylo/logs`). Files are `xylo-<UTC epoch day>.log`, followed by numbered segments. They rotate at the UTC day boundary or 10 MiB, retain seven UTC calendar days, and prune oldest segments to keep total owned log bytes within 100 MiB. Rotation resumes the latest segment after restart. Unrelated files and symlinks are not removed; log files are private (`0600` on Unix).

Provider HTTP failures record protocol, safe model identifier, HTTP status, bounded error code/request ID, elapsed time and tool count. They do not record request/response bodies, prompts, headers or raw provider error text. Turn failures record session/turn ID, failure kind and retryability even when telemetry export is disabled. Worker stderr retains warning/error severity, redacts credential patterns, and discards overlong lines. OTLP remains opt-in.
