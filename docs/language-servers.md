# Native code intelligence

The editor sends full document snapshots and monotonic versions to an application-owned language-server actor. The host queue returns immediately. Native server processes communicate through private, bounded JSON-RPC pipes; no Node or Python language-server package is installed by the application.

The actor orders document open, change and close operations. Completion and hover requests can run concurrently after synchronization. A newer editor version invalidates an older result. Closing a document cancels its pending requests, and the UI discards responses for a different file or version.

## Built-in discovery

Concrete `rust-analyzer` and `clangd` executables are discovered on `PATH`. Rustup shims are skipped when no concrete server is installed. C, C++, Objective-C and Objective-C++ use clangd. Rust initialization disables procedural macros and Cargo build scripts; indexing does not automatically authorize project code execution.

Other languages can use an explicitly configured native server. The application reads `language-servers.json` from its own data directory, never from a project. Each executable must have an absolute path. Arguments are passed directly to the native process without a shell.

```json
{
  "servers": [
    {
      "languages": ["go"],
      "executable": "/absolute/path/to/gopls",
      "args": [],
      "initialization_options": {}
    }
  ]
}
```

Document snapshots are limited to 4 MiB, protocol frames to 8 MiB, header blocks to 8 KiB, pending editor operations to 64, open documents to 64, and server sessions to 16. These bounds protect interactive work and are independent of hardware profiles. Unsupported files remain editable. Assistance requests return an actionable message without showing an error on every keystroke.

The client negotiates UTF-16 positions, matching the editor's completion and hover adapters. Server-initiated workspace edits are rejected until a user explicitly chooses an editor action. The client does not execute language-server commands returned by completion items. Closed servers reconnect on the next editor action. Servers without open documents are released after five minutes, and explicit Quit shuts down all language servers.

## Verification

`cargo test -p workbench-runtime --lib` checks framing bounds, UTF-8 byte counts, configuration boundaries and cancellation before process launch.

An explicit native fixture checks real clangd completion, hover, diagnostics, stale-version rejection and process shutdown:

```sh
cargo test -p workbench-runtime lsp::tests::native_clangd_completion_diagnostics_versions_and_shutdown -- --ignored --nocapture
```

This fixture passed on macOS ARM64 using Apple clangd 21.0.0 on 2026-10-25. That result does not establish native editor acceptance on Windows or Linux. Server packaging, more language catalogs and cross-platform input acceptance remain release work.

Protocol reference: [Language Server Protocol 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/).
