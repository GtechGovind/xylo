# Native desktop app (`crates/desktop`)

## Purpose and boundaries

`crates/desktop` is the native GUI application. Its Cargo package is named **`xylo`** (not `xylo-desktop`) and the binary is `xylo` (`default-run = "xylo"`). It is about 51k lines under `src/` plus a separate validation crate in `validation/`.

The UI is built on `gpui-kit` (pinned `=0.8.1`, with tree-sitter grammars), imported as `extern crate gpui_kit as gpui`. Direct Xylo dependencies are only `xylo-core` (contracts, see [core](core.md)) and `xylo-runtime` (application host, see [runtime](runtime.md)). **The desktop crate does not link the engine, worker or services crates**: agent turns run in the separate `xylo-worker` process that the runtime host supervises (see [worker](worker.md), [engine](engine.md), [services](services.md)). The UI only holds a `HostClient`, sends it bounded commands and receives `HostEvent`s.

Other dependencies: `rfd` (native file dialogs), `tray-icon` (system tray), `image` (PNG only, icon pixels), `lsp-types`, `async-channel`, `shell-words`, `chrono`, `libc`, `tracing`/`tracing-subscriber`.

## Module map

| Path under `crates/desktop/src` | Purpose |
| --- | --- |
| `main.rs` | Entry point: startup options, host spawn, window, tray, menus, quit and reopen handling |
| `startup.rs` | `startup::Command` parser (`--version`, `--help`/`-h`, `--data-dir`, `--debug`) |
| `tray.rs` | OS tray icon and menu; events forwarded over a bounded channel |
| `state.rs` | Pure per-chat live-turn reducer: `TurnState`, `ToolCard`, `Segment`, `Subagent` |
| `tool_presentation.rs` (+`rich_reads.rs`) | Bounded, human-readable tool output prepared per card |
| `ui.rs` | Design tokens (palette, type, radii) and components; `brand_pixels` |
| `icons.rs`, `assets.rs`, `motion.rs` | `AppIcon` catalog, embedded asset source, motion vocabulary and reduced-motion handling |
| `terminal_view.rs`, `terminal_workspace.rs` | Native terminal rendering; project-scoped set of terminals (tabs, splits) |
| `xylo.rs` | The root view `Xylo`: all state, page routing, `HostEvent` dispatch |
| `xylo/*.rs` | One file (or a few) per feature area, all `impl Xylo` blocks, see below |
| `xylo/SETTINGS_DESIGN.md` | Settings design system (measures, type scale, primitives from `settings_ui.rs`) |

Tests live in sibling `tests.rs` files or `tests/` directories (`#[path]` modules), plus `xylo/chat/transcript_flow_tests.rs`.

## Entry point (`main.rs`, `startup.rs`)

1. `startup::Command::parse` runs **before any profile, service or window exists**. It accepts exactly `--version`, `--help`/`-h`, or `--debug` and `--data-dir ABSOLUTE_PATH` (each at most once; a relative path or any other option is an error).
2. `--data-dir` uses `xylo_runtime::AppPaths::at`; otherwise `AppPaths::discover()`. `Observability::init(&paths.data, debug)` follows.
3. `HostClient::spawn_at(paths)` starts the runtime host and returns `Result<(HostClient, Receiver<HostEvent>)>`. A failure is passed into `Xylo::new` instead of aborting, so the window can show a persistent "service gone" banner.
4. `gpui_kit::application()` is configured with `assets::AppAssets` and `QuitMode::Explicit`; `on_reopen` calls `show`.
5. Inside `run`: `set_app_identity(APPLICATION_ID, APPLICATION_NAME)`, `gpui_kit::init`, theme set to Light (the saved preference is applied later by `Xylo::apply_theme`), then the tray, then one window (1380x880, minimum 1000x680, title "Xylo").
6. `ShellState` (a GPUI global) keeps the window handle, the root `Entity<Xylo>` and the tray. Actions `ShowXylo` and `QuitXylo`; `cmd-q` (macOS) or `ctrl-q` binds Quit; an application menu offers "Open Xylo" and "Quit Xylo".
7. Closing the window never exits: with a tray on macOS the app hides, otherwise the window minimizes. **Quit always goes through `Xylo::request_quit`** (see State and persistence). Tray events are drained by a spawned task that calls `show` or `quit`.

If the main window cannot open, the error is logged and `cx.quit()` runs.

## State management

### Root view (`xylo.rs`)

`Xylo` is the single root `Entity` and owns all UI state: the `HostClient` (an `Option`, `None` when the host failed to start), `AppSettings`, the current `Page`, workspace path, the active chat `session: Uuid`, conversation list and paging, loaded `messages`, `turns: BTreeMap<Uuid, TurnState>`, the composer (`TextareaState`), the file editor (`EditorState`) and many feature sub-structs (`DraftUi`, `DocumentUi`, `UpdateUi`, ...). Each feature file adds an `impl Xylo` block and its own `*Ui` struct held as a field.

`Page` variants: `Chat`, `Permissions`, `Skills`, `Files`, `Search`, `Drafts`, `Voice`, `Images`, `Notifications`, `Changes`, `Artifacts`, `Memory`, `Connectors`, `Plugins`, `Environments`, `Providers`, `Schedules`, `Appearance`, `Assets`. `Page::is_settings` marks the pages that replace the sidebar with settings navigation. `Page::key`/`from_key` persist the active page in `UiPreferences::active_page`; transient pages (drafts, voice, images, notifications, artifacts, schedules) **reopen as the chat**.

### Per-chat live turn reducer (`state.rs`)

`TurnState::apply(EngineEvent)` is a pure reducer, one `TurnState` per chat, so **engine events cannot leak between selected chats**. A turn is a chronological list of `Segment`s: `Text`, `Thinking`, `Tools(ids)`, `Subagent(id)`, `Note`. Tool calls carrying `parent_id` fold under a `Subagent` row of the `agent_delegate` call. Every terminal event settles unfinished cards (`not_completed`), so nothing spins forever. Streamed text is grown in a private buffer and rebuilt into a `SharedString` at most once per frame (`flush_text`). `render_revision` and `take_changed_rows` let the history list re-measure only changed rows. `TurnOutcome` is `Completed`, `Stopped` or `Failed(TurnFailure)`. `ToolCard` carries `approval`, `rule` ("always allow" scope), `reason` and `decision_lost`. The events `WorkspaceChanged`, `CustomToolRequested`, `McpElicitation*`, `Ready`, `TitleUpdated`, `Steering` and `ProviderCredentialRequested` are ignored by the reducer and handled elsewhere. `ToolCard::presentation` comes from `tool_presentation.rs`: a bounded diff, command, excerpt or summary; **raw model-facing envelopes are never rendered**, only copyable.

### Persistence

The UI does no database or file I/O itself. Durable state lives behind the host: settings (`HostEvent::Settings`), drafts (`xylo/drafts.rs`, revision-correlated), turn queue and document drafts. Layout preferences (sidebar and work-panel widths and open state, open projects, recent models) are in `AppSettings::ui` (`UiPreferences`, see [core](core.md)). Editor saves go through `HostClient`: `pending_saves` holds the saved text, and `HostEvent::Saved` clears `editor_dirty` only if the editor still equals that text.

## Talking to the runtime and worker

- **Receive.** `Xylo::new` spawns a task on the receiver of `HostEvent`s. It drains up to 64 queued events at a time and **merges adjacent `EngineEvent::Delta`s** of the same request and session (at most 64 KiB per merged delta); tool, approval and status boundaries are never merged. Events are applied on the UI thread by `Xylo::event`, with a roughly 2 ms budget per batch before yielding.
- **Dispatch.** `Xylo::event` is one large `match` over `HostEvent` (permissions, plan modes, drafts, queue, document drafts, history, workspace opened/indexed, file saved, LSP responses and diagnostics, settings, models, git status and diffs, schedules, usage, `Stopped`, ...). The full variant list is in `crates/runtime/src/host.rs` (see [runtime](runtime.md)).
- **Send.** `host_call` runs a `HostClient` method and turns an error into a product message; with no host it reports "The application service is unavailable. Restart Xylo to reconnect." Worker commands go through `engine_command`, which calls `HostClient::send_to(session, WorkerCommand)`. `HostClient` methods only enqueue bounded commands and return immediately.
- **Foreground hint.** Window activation changes call `HostClient::foreground(active)`.
- **Test seam.** In `cfg(test)`, `validation_worker_commands` replaces `send_to` with a bounded channel, so tests observe the exact `WorkerCommand`s without a worker.

### Approvals and questions

Routine autosaves do not disable the composer. Sending, steering, retrying, or sending a queued message cancels a pending close/update continuation; typing also cancels pending close intent while autosaving continues. Draft, Memory, and document caches evict clean entries when full and retain unsaved entries without blocking navigation.

`chat.rs::decide_action` records the decision on the card immediately and sends `WorkerCommand::Approve { action_id, allow, remember }`; the engine still enforces the outcome. **An approval whose change diff was truncated (`diff_truncated`) is never allowed from the UI.** `ask_user` and `exit_plan_mode` cards are not approvals: they answer via `answer_interaction` (`InteractionAnswer`), see `chat/question_card.rs` and `chat/plan_card.rs`. The keyboard shortcut to allow the oldest pending action applies only when the chat page is in front with nothing layered above it and focus is in the composer or transcript (`approval_shortcut_applies`). MCP elicitation forms (`mcp_forms.rs`) are filled by the user, **never by the model**.

## Main panels

| Area | Files | Notes |
| --- | --- | --- |
| Sidebar | `sidebar.rs`, `history_search.rs` | Conversation list with paging and placeholder rows until the first list arrives; Cmd-K chat search palette |
| Chat and transcript | `chat.rs`, `chat/*`, `history.rs`, `transcript.rs`, `chat/thinking.rs`, `chat/tools.rs`, `tool_result_view.rs` | Variable-height history list followed by the live turn rows; composer, attachments, drafts, queue |
| Work panel | `work_panel.rs`, `git_review.rs`, `shell_jobs.rs`, `file_browser.rs`, `file_tree.rs`, `project_search.rs` | Tabs `changes`, `files`, `terminal` and, when the chat has helpers, `subagents` |
| Editor | `editor_assist.rs`, `editor_*` fields in `xylo.rs` | `gpui_kit` `Editor` with language-service completion, hover and diagnostics |
| Terminal | `terminal_view.rs`, `terminal_workspace.rs` | Native; no webview |
| Settings | `settings.rs`, `settings_ui.rs`, `providers.rs`, `model_picker.rs`, `permission_control.rs`, `accounts.rs`, `memory*.rs`, `extensions*.rs`, `plugins*.rs`, `environments*.rs`, `speech.rs`, `image_generation.rs`, `notifications.rs`, `updates.rs`, `assets.rs`, `appearance.rs` | Built from `settings_ui.rs` primitives |
| Documents | `documents.rs` and `documents_*.rs` | Imported office documents with source revision and structured edits |
| Other | `usage.rs`, `tasks.rs` (schedules), `worktrees.rs`, `learning.rs`, `context_panel.rs`, `quit.rs`, `app_status.rs` | Usage, scheduled tasks, chat worktrees, status toast |

### Chat and transcript

The live turn renders as rows: a work summary row, one row per `Segment`, then a footer (`transcript.rs`). `history.rs` splices older pages in without moving the visible message, pauses tail following when the reader scrolls up and offers "Jump to latest". Finished-turn text is cached as shared strings so rows do not copy it each frame. Send admission is correlated by receipt: `drafts.rs` has a deadline after which the composer unlocks and offers retry, yet **a late receipt is still honoured so an admitted message is never shown twice**. Admission errors are mapped to fixed product copy and **never echo provider bodies, URLs, paths or credentials**.

### Work panel and editor

The work panel opens only on request or when a file or diff is opened, and never on other pages. The Changes tab shows a virtualized Git folder tree; refreshes keep the selected file and its diff. Shell jobs show a bounded output tail with Stop. The editor tracks `editor_path`, `editor_dirty` and `pending_saves`; language-service work (`editor_assist.rs`) runs in background tasks with version counters, and a stale response is dropped. Editing cancels pending draft, memory-form and document-draft flush timers so one flush owner runs at a time.

### Terminal

`TerminalWorkspace` is one per project; `terminal_cache` keeps parked workspaces for other projects. `TerminalSession::spawn` (from `xylo_runtime::terminal`) runs on `background_executor`; starts are fenced so a late spawn cannot recreate a closed workspace. `TerminalView` handles glyph shaping, selection, IME, scrollback search, text size (10 to 24) and an "Ask Xylo" panel. **That panel opens a separate saved chat and grants no authority to send bytes to a shell.** PTY parsing is in the runtime crate.

### Documents and recovery

Document parsing and writing run in the supervised helper via host capabilities, never the render thread; reads carry `epoch`/`generation` so stale replies are ignored, and explicit actions use `confirm_explicit_action` (no second confirmation in Full access), while unsaved-edit and conflict choices use `window.prompt`. Tool activity is transient; interrupted actions have no durable action journal, review UI or recovery gate. Saved chat checkpoints can retain tool errors, while unanswered calls are reported as interrupted without automatic replay. Live activity and saved command output do not block chat submission, navigation, or shutdown. Ordinary document autosaves continue across project switches; oversized uncaptured edits, conflicts, and pending copy/relink operations still require a decision or completion.

## Icons, theme and motion

- **Theme (`ui.rs`).** Colours are functions (`bg()`, `panel()`, `text()`, `muted()`, `brand()`, `success()`, `danger()`, ...) that read a global dark flag (`set_dark`, `is_dark`). One indigo accent marks the action to act on, mint marks success. Toolkit primitives are reused for behaviour only. `Xylo::apply_theme` (`xylo/appearance.rs`) applies `AppSettings::theme`: `"light"`, `"dark"` or `"system"`; `"system"` hands the window appearance back to the OS and keeps following changes. `main.rs` starts Light until settings load.
- **Icons (`icons.rs`, `assets.rs`).** Icons are named by meaning with `AppIcon`, not by glyph; one concept maps to one glyph, in two sizes. `AppAssets` embeds only listed SVGs (the toolkit defaults plus extras) and rewrites the root `stroke-width` to 1.5 for outline icons. The brand mark is `assets/xylo-mark.svg`; `ui::brand_pixels(size)` draws it in Rust for the Linux window icon and the tray.
- **Motion (`motion.rs`).** Helpers last 120 to 240 ms, never delay input, and honour the OS "Reduce motion" setting, which is re-read when the app becomes active (macOS posts no change event).
- **Settings layout.** `xylo/SETTINGS_DESIGN.md` is the authority: pages use `settings_ui.rs` primitives (220 px sidebar, 680 px content column, 56 px rows) and set no paddings of their own.

## Concurrency and UI-thread rules

- **Nothing blocking runs on the UI thread.** Database, credential, process, network, document and LSP work goes through `HostClient` (which enqueues and returns) or `background_executor()` tasks. Module docs state this for drafts, documents, editor assist, terminals, speech, native tools and updates.
- Results return as UUID-correlated `HostEvent`s (`request_id`). Late or superseded replies are checked against an epoch, generation or expected id and dropped or reconciled, never applied blindly.
- The OS tray callbacks only `try_send` into a bounded channel (capacity 8); there is no polling.
- Debounced work (Memory search, 200 ms) is a replaceable `Task`: replacing it cancels the earlier one.
- Streaming text is coalesced (see above) so repaint cost stays bounded.

## Quit, update and shutdown

`quit.rs`: quitting asks first if tasks run or await approval in guarded modes; Full access honors Quit immediately. Editors, drafts, memory and document edits are still saved, with a choice for unsaved editor changes. A stalled save shows a banner naming what is unsaved with **Quit anyway**, so the user can always leave. Shutdown then waits for the host to report it has finished. Installing an update (`updates.rs`) closes Xylo, so open project shells are named in a confirmation in guarded modes; Full access honors the install action without asking again. The UI receives only safe update summaries; verified offers and installer bytes stay in the host.

## Security and privacy

- Secrets are entered in masked inputs and handed to the host (`save_provider_connection`); the UI keeps only credential references. A save is not trusted until the host answers.
- Account tokens stay in the host; account actions are UUID-correlated (`accounts.rs`).
- Product error copy is allow-listed (`drafts.rs`); arbitrary service text is not echoed.
- Approvals, questions and MCP forms are answered only by explicit user action (see above).
- `--data-dir` must be absolute. SSH/container environments (`environments.rs`) are explicit saved identities; neither cancellation nor a saved flag counts as success.

## Platform specifics

- **macOS.** Tray icon is an alpha template; with a tray, closing the window hides the app (`cx.hide()`); `cmd-` chords; reduced-motion and appearance are re-read on activation.
- **Linux.** `app_id = "xylo"` for Wayland lookup and X11 grouping, pixel window icon; the tray uses the `ksni` feature of `tray-icon`.
- **Windows.** Release builds use the GUI subsystem (`windows_subsystem = "windows"`). `terminal_view.rs` has a Windows branch for the preferred shell.
- **Other platforms.** Without a tray, closing the window minimizes it. Shortcuts use `ctrl` instead of `cmd` (`shortcuts.rs`).

## Validation harness (`crates/desktop/validation`)

`xylo-native-validation` is a **separate Cargo workspace** (its own `[workspace]` and `Cargo.lock`), not a member of the root workspace. It depends on `xylo-core`, `xylo-runtime`, `xylo-services` and `gpui-kit` with `test-support`. It does not depend on the app crate: each test file pulls the app sources in with `#[path = "../src/....rs"] mod ...;` and `include!("../src/xylo.rs")` (see `interactions.rs`), so it compiles the production `Xylo` view with the `validation` feature on. That feature (default in this crate, never enabled in the app) exposes test-only accessors such as `TerminalWorkspace::session_count`, `has_live_sessions`, `active_view` and `documents_validation_focus_grid`.

Two kinds of targets are declared in `validation/Cargo.toml`:

- **Interaction tests** (`native-interactions`, `workspace-`, `updater-`, `draft-`, `vision-`, `notification-`, `memory-`, `image-generation-`, `document-`, `management-`, `recovery-`, `document-recovery-`, `terminal-workspace-`, `file-tree-`, `account-`, `composer-interactions`). They use `#[gpui_kit::test]` with `TestAppContext`, open a real window with `Xylo::new(Err(...))` (no host) and drive real controls.
- **Rendering captures** (`harness = false`: `native-rendering`, `settings-rendering`, `recovery-rendering`, `document-recovery-rendering`, `terminal-rendering`, `composer-rendering`, `file-tree-rendering`, `extension-rendering`, `portable-tool-rendering`, `paste-rendering`). `capture_dir::fresh(harness)` clears and returns `target/native-preview/<harness>` (or `$XYLO_RENDER_DIR/<harness>`), so a rerun never meets old evidence.
- `native-performance` is a binary requiring the `performance` feature.

Shared fixtures: `recovery_fixture.rs`, `document_recovery_fixture.rs`, `fixtures/` (includes `management-container.sh`).

## Testing

Unit tests live beside the code (`state/tests.rs`, `startup/tests.rs`, `assets/tests.rs`, `icons/tests.rs`, `motion/tests.rs`, `xylo/**/tests.rs`, `xylo/chat/transcript_flow_tests.rs`). A count of `#[test]` and `#[gpui_kit::test]` attributes found about 164 and 37 in `src/`, and about 152 `#[gpui_kit::test]` in `validation/*.rs`.

Commands (**not run while writing this document**):

- `cargo test --locked -p xylo <filter>` for the app crate. The package name is `xylo`; the workspace root `Cargo.toml` should be checked for how `-p` resolves.
- `cargo test --locked --manifest-path crates/desktop/validation/Cargo.toml --test native-interactions` (any target name above can replace it; the validation crate has its own lockfile).

`transcript_flow_tests.rs` drives approvals end to end through the `validation_worker_commands` probe and asserts the exact `WorkerCommand::Approve` values.

## Related

[runtime](runtime.md) (host, terminal, LSP, updates), [core](core.md) (contracts), [worker](worker.md) and [engine](engine.md) (the process behind `WorkerCommand`/`EngineEvent`), [services](services.md) (storage).

## Gaps

- Read in depth: `main.rs`, `startup.rs`, `tray.rs`, the head of `state.rs`, `xylo.rs` (struct, `Page`, event pump, `engine_command`), `decide_action` in `chat.rs`, module doc comments of most files, `Cargo.toml` of both crates.
- **Only skimmed from headers or doc comments:** `settings*.rs`, `providers.rs`, `model_picker.rs`, `sidebar.rs`, `extensions*`, `plugins*`, `environments*`, `memory*`, `speech.rs`, `image_generation.rs`, `learning.rs`, `tasks.rs`, `usage*`, `documents*` internals, `terminal_view.rs` internals, `motion.rs`.
- The `HostEvent` variants were not enumerated beyond those seen in `Xylo::event`.
- Test counts are from `grep` of attributes, not from running tests; no test, build or validation harness was run.
- How `-p xylo` resolves in the root workspace and CI wiring of the validation crate were not checked.
