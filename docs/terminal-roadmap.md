# Modern native terminal

Research reviewed on 2026-10-25. This document describes the next terminal milestone; the current preview has a working native terminal, with a basic interface. The modern session interface and AI actions below are planned work.

## Direction

Keep the existing Rust `alacritty_terminal` core and `portable-pty` process layer while building an original GPUI terminal workspace. This preserves the native architecture and existing shell support. Compare alternative cores against the same workloads before making a performance claim or changing dependencies.

The interface, shell integration and AI workflow need their own implementation. Adopting another terminal parser would still leave those layers to build.

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

## Planned experience

- Session tabs, split panes, readable shell labels and working-directory context.
- Theme-aware colors, spacing and typography, font zoom, focused search and useful exit states.
- Searchable command history and command/output blocks when shell integration supplies real command boundaries, duration and exit status.
- **Ask Xylo**, **Explain selection**, **Explain failure** and editable command proposals using the user's selected local or cloud model.
- Copy or insert a proposed command without submitting Enter. Agent execution continues through normal permissions, action records and cancellation.
- Clickable file locations and links, with native selection, clipboard, Unicode and IME behavior.

Shell metadata comes from session-local integration, such as [OSC 7 and command markers](https://ghostty.org/docs/features/shell-integration). Unsupported shells keep ordinary terminal behavior. Prompt text alone cannot establish whether a command finished successfully. A saved transcript is separate from a live shell process.

AI actions use a bounded, inspectable excerpt selected by the user. Opening a terminal or typing a command does not invoke a model or upload its scrollback automatically. Terminal output remains untrusted input; it cannot grant permissions or authorize commands.

## Performance work

The current implementation builds a complete visible snapshot after each PTY read, then shapes all visible rows for rendering. Delivery through a coalescing channel does not eliminate that allocation work.

The next implementation should parse every byte off the UI thread, publish only the latest dirty display state at a bounded frame cadence, and cache unchanged rows and text shaping. Terminal protocol replies and user input must remain responsive independently of repaint scheduling. Idle sessions should schedule no periodic rendering work.

Keep bounded queues, scrollback, retained command metadata and snapshots. Store larger history on disk with paging. Closing a terminal must terminate and reap its shell without blocking the UI.

## Acceptance

Measure input-to-paint latency, frame time, allocation rate, output throughput, idle CPU, process-tree memory, resize/reflow and large-history search. Use identical fonts, viewport, build mode and workload for each core; record source, binary, hardware and raw samples.

Qualify real shells on macOS, Windows and Linux, including Vim/tmux, alternate screens, application mouse reporting, modified navigation keys, multiline/bracketed paste, CJK/emoji/combining text and IME. Provider fixtures must verify bounded context, cancellation, stale-result rejection and that generated commands do not execute automatically.

Modern terminal acceptance remains separate from the existing native test and release results in [verification](verification.md).
