# Modern native terminal

Research reviewed on 2026-10-25. The native terminal now includes session tabs, two-pane splits, font zoom, on-demand search and an editable **Ask Xylo** handoff to a new chat. The remaining workflow and qualification work is listed below.

## Direction

Keep the existing Rust `alacritty_terminal` core and `portable-pty` process layer while building an original GPUI terminal workspace. This preserves the native architecture and existing shell support. Compare alternative cores against the same workloads before making a performance claim or changing dependencies.

The interface, shell integration and AI workflow have separate responsibilities. Adopting another terminal parser would still leave those layers to build.

## Implemented interface

The terminal uses a compact session strip and toolbar. A selected tab identifies the active session; split panes have a neutral divider. The interface labels the folder in which each workspace was opened, without treating that label as live shell-directory metadata. Closing a live tab asks before stopping its shell.

The renderer selects a concrete monospace font and uses the same metrics for text, cursor, selection and composition placement. Unicode clusters are anchored to their parser columns. A native macOS font/GPU regression reproduced a 163.60-point gap with the former generic font choice and measured zero gap for the corrected ASCII prompt. This does not qualify every font, IME or platform.

**Ask Xylo** offers an editable excerpt from the explicit selection or visible viewport, capped at 12 KiB plus an omission marker. Continuing opens a separate saved chat while retaining the existing draft. The normal model, send, permissions and cancellation flow applies. Opening the assistance editor or continuing to chat does not submit a turn or write a generated command to the PTY.

## Native options reviewed

| Project | Reusable part | Decision for Xylo |
| --- | --- | --- |
| [Alacritty](https://docs.rs/crate/alacritty_terminal/0.25.1/source/Cargo.toml) | Apache-2.0 Rust terminal state and VT parser, already integrated. | Initial core; improve rendering and protocol coverage around it. |
| [Ghostty VT](https://github.com/ghostty-org/ghostty/blob/cec8b92f1ecd176a20a56aa405021e7ae991b996/include/ghostty/vt.h) | MIT Zig/C terminal core and custom-renderer state; the public API is still unstable. | Benchmark candidate behind an adapter. Its VT library supplies no GPUI interface or GPU renderer. Ghostty's separate internal embedding header targets its macOS app. |
| [Rio VT](https://github.com/raphamorim/rio/blob/59203ccd5fa1e6b760a738c1ad653bb29f2f3465/rio-vt/README.md) | MIT Rust state/parser core. Its PTY feature can be disabled. | Strong Rust comparison candidate. Retain one PTY owner and our GPUI renderer. |
| [WezTerm](https://github.com/wezterm/wezterm/blob/e2a98e1b8af156988ad0ac778658ac27b60a55fb/term/README.md) | MIT Rust emulator core with separate GUI and process layers. | Useful core alternative and multiplexing reference; adopting its whole application adds unnecessary integration scope. |
| [Zed](https://github.com/zed-industries/zed/blob/a5c4ca8bf6c4717a328a4a61b42d6d1d38949bcc/crates/terminal_view/Cargo.toml) | GPUI terminal integration over Alacritty; terminal/view crates declare GPL-3.0-or-later. | Architecture and interaction reference. These crates have a different license from GPUI itself. |
| [Warp](https://github.com/warpdotdev/warp/blob/1e6c3ae465d6a925a9b97ae03877aaadd159b8f1/README.md) | Open-source native client: AGPL-3.0 application, MIT `warpui` and `warpui_core`. | Reference for command blocks and AI interaction. Its client is a separate application framework, with a different license from our application code. |

These are source and API findings, not measured speed rankings or completed Xylo integrations. Third-party license notices remain required for any adopted dependency.

## Remaining experience

- Live working-directory context and richer shell exit metadata.
- Broader theme, font, accessibility and input qualification.
- Searchable command history and command/output blocks when shell integration supplies real command boundaries, duration and exit status.
- Command-aware **Explain failure** and editable command proposals using the user's selected local or cloud model.
- Copy or insert a proposed command without submitting Enter. Agent execution continues through normal permissions, action records and cancellation.
- Clickable file locations and links, with native selection, clipboard, Unicode and IME behavior.

Shell metadata comes from session-local integration, such as [OSC 7 and command markers](https://ghostty.org/docs/features/shell-integration). Unsupported shells keep ordinary terminal behavior. Prompt text alone cannot establish whether a command finished successfully. A saved transcript is separate from a live shell process.

AI actions use a bounded, inspectable excerpt selected by the user. Opening a terminal or typing a command does not invoke a model or upload its scrollback automatically. Terminal output remains untrusted input; it cannot grant permissions or authorize commands.

Proposals stay in an editable command buffer. Insertion into a live terminal is limited to a validated single line at a known shell prompt; multiline content and control characters stay in the editor for explicit review. Alternate-screen applications keep normal terminal input.

## Performance work

The parser processes every byte independently of display scheduling. A dirty-state publisher coalesces viewport construction at a 16 ms cadence, waits without periodic polling when idle, and flushes the complete tail on EOF/error. Parser replies are enqueued after releasing the terminal lock, preserving replies under backpressure.

A local debug scheduling replay compared six alternating matched pairs with the same 2 MiB input, 8 KiB packets and 120 × 40 viewport. Median viewport constructions fell from 257 to 5; median accumulated construction time fell from 74.75 ms to 1.48 ms. Whole-burst p95 remained approximately 176 ms in both paths. These samples measure a synthetic parser/snapshot workload, not interactive latency, GPU frames, release performance or a comparison against another emulator.

Cache unchanged rendered rows and qualify input-to-paint performance separately. Terminal protocol replies and user input must remain responsive independently of repaint scheduling.

Keep bounded queues, scrollback, retained command metadata and snapshots. Store larger history on disk with paging. Closing a terminal must terminate and reap its shell without blocking the UI.

## Acceptance

Measure input-to-paint latency, frame time, allocation rate, output throughput, idle CPU, process-tree memory, resize/reflow and large-history search. Use identical fonts, viewport, build mode and workload for each core; record source, binary, hardware and raw samples.

Qualify real shells on macOS, Windows and Linux, including Vim/tmux, alternate screens, application mouse reporting, modified navigation keys, multiline/bracketed paste, CJK/emoji/combining text and IME. Provider fixtures must verify bounded context, cancellation, stale-result rejection and that generated commands do not execute automatically.

Modern terminal acceptance remains separate from the existing native test and release results in [verification](verification.md).
