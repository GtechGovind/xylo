# Native desktop UI

The desktop is a Rust GPUI application pinned to `gpui-kit 0.8.0` / `gpui-pre 0.3.9`. The desktop links the lightweight runtime/client boundary; native execution engines live in the worker/helper processes. All service calls enqueue work and results arrive through an awaited bounded channel. There is no continuous idle UI polling. Asset downloads, recording and application update progress use bounded status timers only while their operations are active; completion, cancellation and interruption stop them. Project scans, native document/audio pickers, PTY startup and native services run away from the render thread.

## Product surfaces

- A light sidebar with left-aligned, full-width New Chat, Search, Scheduled Tasks, Teams, Projects, Conversations and Settings rows.
- A centered welcome page, assistant prompt templates, rounded composer, model choice and per-action approval controls. Chat text streams incrementally; cancelling retains partial text. Native variable-height lists retain every loaded message and the visible-message/pixel anchor while earlier pages are prepended. Bodies load by 100-message append-sequence pages; conversation metadata uses updated-at/UUID pages and a Load more row. Sidebar/project rows and discovered models are virtualized. New Chat leaves existing tasks running.
- Model settings for custom/cloud providers, OpenAI Responses, Anthropic, Gemini, Azure OpenAI, Vertex AI, Amazon Bedrock, Ollama, LM Studio and llama.cpp. Native browser sign-in connects a ChatGPT account only after the host validates callback, identity and required access; tokens never enter UI state. Models remain an explicit choice. Downloads offers verified feature/model packages with size, license, install progress, cancellation and a separate Use action. Runtime/library paths, RAM selectors, hardware profiles and manual token-limit controls do not appear in the UI.
- A virtualized full file list and Git diff, native code editor, generated artifacts, searchable project memories and connected tools. File scans respect ignore rules, run in the background and have no arbitrary row/depth cap. Refresh runs on file-tab entry and on request. Save acknowledgments only mark the editor clean when they correspond to its current content. GPUI Kit completion menus, hover popovers and diagnostic decorations use the native language service; edits debounce for 250ms, documents have monotonic versions, requests are bounded and obsolete work is cancelled.
- Direct native browser, document, speech and screen-capture actions with file pickers, progress, cancellation and human-readable results. PDF/speech/local inference use explicitly installed assets from Downloads; an installed browser is discovered automatically. OS capture permission is checked when used. Document/audio pickers operate within the selected project's authorization boundary.
- Scheduled tasks use durable host scheduling; Teams shows actual delegated-task history. Neither page invents demo entries. Light/dark theme, sidebar/file panel widths, terminal height/visibility and active page persist through typed settings. Native split handles and accessible size controls share these preferences. App shortcuts include save, new chat, search, terminal and editor suggestions; printable terminal text and macOS Option composition pass through the native input handler.
- A native terminal backed by a real PTY and Alacritty parser: shaped text, ANSI colors, Unicode/combining characters, selection, clipboard, scrollback search, resize, bracketed paste and a native IME input handler. The terminal is lazy and event driven.

The window-close handler retains the window/entity so background work and event consumption remain alive. On macOS it hides the application when the tray is available; elsewhere it minimizes the window. Native tray Open/Quit and the application Quit shortcut are explicit. Quit first prompts Save/Discard/Cancel for unsaved editor text. Save waits for a matching save acknowledgment, and newer edits prevent shutdown. Quit then requests host shutdown, waits for `Stopped`, and has a 30-second emergency deadline. Linux/Windows tray behavior and OS IME need separate platform acceptance; a macOS compile does not validate those platforms.

## AionUi reference and attribution

Reference reviewed through the GitHub connector on 25 October 2026 at commit `11cd2a9a20d1e033157636efc570b41384c66d85`:

- [Home screenshot](https://github.com/iOfficeAI/AionUi/blob/11cd2a9a20d1e033157636efc570b41384c66d85/resources/homepage.png)
- [Model settings screenshot](https://github.com/iOfficeAI/AionUi/blob/11cd2a9a20d1e033157636efc570b41384c66d85/resources/llm_newapi.png)
- [Default color scheme](https://github.com/iOfficeAI/AionUi/blob/11cd2a9a20d1e033157636efc570b41384c66d85/packages/desktop/src/renderer/styles/themes/default-color-scheme.css)
- [Welcome-page layout](https://github.com/iOfficeAI/AionUi/blob/11cd2a9a20d1e033157636efc570b41384c66d85/packages/desktop/src/renderer/pages/guid/index.module.css)
- [Settings navigation](https://github.com/iOfficeAI/AionUi/blob/11cd2a9a20d1e033157636efc570b41384c66d85/packages/desktop/src/renderer/pages/settings/components/SettingsSider.tsx)

Presentation colors and layout are native adaptations of the AionUi source: Copyright 2025 AionUi (aionui.com), [Apache License 2.0](https://github.com/iOfficeAI/AionUi/blob/11cd2a9a20d1e033157636efc570b41384c66d85/LICENSE). This application changes the rendering, state and event implementation to Rust/GPUI. AionUi's logo, engine logos and product artwork were not copied. The repository includes the Apache-2.0 license.

The comparison corrected the original dark palette, centered navigation, composer height/footer alignment and assistant-card alignment. Shared values include the 248px sidebar, 34px navigation rows, 24px composer radius, 800px content maximum and light/lavender color tokens. Current screenshots are `native-preview/home.png`, `settings.png`, and `live-home.png`.

This is not a pixel-identical copy. It retains Workbench branding, six available task templates and four task categories; it does not display uninstalled engines as though they were connected. Provider setup uses a simple inline form rather than AionUi's platform-selection modal. Teams currently represents delegated tasks, rather than AionUi's named team workspaces. These are visible product differences, not verified matches.

## Verification

Normal workspace check and unit tests:

```sh
cargo check -p agent-workbench
cargo test -p agent-workbench
cargo test -p workbench-runtime terminal::tests
```

Native interaction/render verification uses an isolated test manifest because the upstream toolkit's optional test dependencies require `cocoa 0.26.0` and `unicode-properties 0.1.3`, while the ordinary workspace resolves later compatible patches. Its own lock avoids altering the shipped dependency graph:

```sh
CARGO_TARGET_DIR="$PWD/target" WORKBENCH_RENDER_DIR="$PWD/docs/native-preview" \
  cargo test --manifest-path crates/desktop/validation/Cargo.toml --locked
```

The validation targets render the production view and exercise native buttons, rather than an HTML mock. The rendering target runs on the main thread and requires real Metal on macOS or WGPU on Linux. It generates 2760×1760 home/settings captures from a 1380×880 logical window. Windows offscreen rendering is unavailable in the pinned GPUI platform; portable interaction tests remain distinct.

On macOS arm64, the native interaction suite and actual Metal captures passed. Runtime terminal tests passed, including a real shell printf round trip, terminal resize and scrollback search. The running macOS app's sidebar alignment was also inspected through native accessibility and an actual 2294×1574 window screenshot. CUA can return a small Stage Manager thumbnail when the app is hidden; raise the specific window and verify image dimensions before presenting a saved screenshot as full-size.

Real provider accounts, local model assets, PDFium, speech voices, every IME/layout, Windows/Linux packaging and task continuation under native tray behavior require their own acceptance evidence. The UI's presence or an empty setup state does not establish those capabilities were exercised.


## Requirements and current evidence

These are separate evidence levels. A passing render or fixture does not establish an authenticated provider account, installed model quality, every desktop platform, or release readiness. See `completion-audit.md` for the repository-wide release gates.

| Requirement | Implementation and evidence | Remaining acceptance |
| --- | --- | --- |
| AionUi layout, left-aligned sidebar, product-only controls | Shared native navigation rows; original reference comparison above; fresh Metal home/settings renders. Downloads hides runtime dependency cards and presents product descriptions instead of build/configuration detail. | Latest full live window comparison after unlock; listed branding/content differences remain. |
| Full history, paging, scroll retention | Native interaction test renders a 2,000-message conversation with fewer than 40 message controls in the frame, reveals the earliest message, and pauses tail following. A second test prepends 80 older messages, removes overlap and retains the same anchor with its 12px offset. Metadata pages merge by ID and expose Load more. | Stress large real archives and measure frame-time percentiles; database-wide title/body search and exact message windows now have a real Host/native-control fixture; large-archive latency/frame percentiles remain unmeasured. |
| Full files/diff/model lists | Native uniform lists; no 1,000-file, ten-level, 10,000-diff-line or 200-model display cutoff. Filesystem test checks 1,200 files plus a twelve-level path and ignore rules. | Native watcher create/rename/delete/switch and real UI external-change/Unicode-search fixtures pass on macOS. Large-workspace event storms and other operating systems remain unqualified. |
| Native LSP completion/hover/diagnostics | Explicit OS qualification test starts the real host in a temporary profile and actual native clangd. Production GPUI completion menu receives `count`, diagnostics decorate the editor, and a native pointer move opens a hover popover. Requests use UTF-16 positions, bounded channels, version checks and cancellation. | Other languages/platforms; Rust analyzer package install/use and project-specific configurations. |
| Persisted layout and themes | Native test restores dark theme and stored sidebar width/page; Light button changes palette; keyboard size controls and a real split-handle drag update the same settings. Core/host store typed preferences. | Live multi-monitor scale/window lifecycle and platform title-bar checks. |
| Keyboard, Unicode and IME | Native composer test exercises preedit and Japanese/emoji commit without submitting a task. Terminal unit tests cover controls and defer printable characters to native input. Labels are attached to buttons, composer, editor, search and terminal. | Physical OS IMEs, keyboard layouts, screen reader navigation and Windows/Linux accessibility audits. |
| Asset setup and explicit model choice | Catalog contract test accepts actual nullable metadata, exposes an Install action, hides internal runtime dependencies and confirms a catalog read starts no install. Helper owns verification, installation and activation. `interrupted` is a terminal job state; no idle status timer. | Real downloads/install/activation results belong to the helper evidence; native UI download/error/retry acceptance after unlock. |
| Account/provider setup | Correct family-specific presets and model choices; native sign-in/cancel/sign-out controls. Account display requires both `connected` and required access. API key input is hidden for an account credential, and switching providers clears the draft key. Azure needs a deployment name; Vertex needs project/location and publisher model ID. | Human account login and real inference; fixture tests are not account acceptance. |
| Approvals, background tasks, save/quit | Native tests exercise New Chat retaining a running task, Save/Discard/Cancel cancellation and save failure/newer-edit protection. Tray close retains the view and explicit Quit waits for host shutdown. | Live native close/reopen/tray/active-task continuation check remains blocked by the locked Mac. |
| Terminal | Real PTY/parser tests cover process output, resize, search, Unicode, selection and bracketed paste; GPUI supplies native rendering/input. | OS IME, interactive TUI and per-platform terminal qualification. |

Latest desktop evidence: `cargo check -p agent-workbench`; strict all-target desktop Clippy; 8 normal desktop unit tests; 18 passing main native validation tests plus the separate workspace/speech interaction target and one explicitly separate native-clangd qualification test. The qualification test is ignored by default because it requires an installed OS language server. Run it explicitly:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test \
  --manifest-path crates/desktop/validation/Cargo.toml --locked \
  --test native-interactions native_editor_completion_uses_real_host_and_clangd \
  -- --ignored --nocapture
```

The native-I/O qualification test enables GPUI test scheduler parking intentionally; deterministic interaction tests retain their normal scheduler. Native Metal captures are separate from the interaction harness and do not require unlocking the interactive app window. `live-home.png` is the earlier inspected live window, not a claim that the latest added pages received a new live check.


## Search, speech and application updates

Project search uses immutable native file-index snapshots and bounded virtualized result pages. Requests can be stopped; a changed index invalidates pagination. Opening a match uses a correlated file read and converts its UTF-8 byte column to the native editor character position. File events refresh a clean editor and preserve its cursor. Unsaved edits survive external changes; replacing the disk file requires an explicit Overwrite action. Long path labels truncate so Save remains reachable.

Conversation search covers saved titles and bodies, independently of the sidebar cache. It opens a bounded window centered on the persisted source message, with earlier/later/latest navigation. Source and request-cursor checks reject stale replies. See [history search](history-search.md).

Voice has native recording, local transcription, explicit transcript review, selected voice synthesis and explicit playback. Speech results never send a chat automatically. Device access requires a user action. The app exposes a recording indicator and bounded status updates; actual microphone/output permission acceptance remains pending.

Application updates expose explicit Check, Download and Install controls. The host retains authenticated offers and staged files. Install requires idle tasks/helpers, closed terminal and finished speech work, saved file edits and no unsent message draft. Failed admission retains the terminal and editor. The preview's update policy remains disabled until actual publisher trust and a release feed are supplied. See [distribution](distribution.md).
