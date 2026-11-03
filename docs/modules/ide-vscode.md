# Former VS Code IDE bridge

The bundled `ide/vscode` extension has been removed. The following records its previous protocol and implementation; its build and launch commands no longer apply to this checkout.

## Purpose and boundaries

`xylo-ide-bridge` (version 0.1.0, `private`, VS Code `^1.96.0`) is a development extension that exposes the current selection, diagnostics and open-editor metadata of a **trusted, local** VS Code workspace through a local MCP server over WebSocket, and can open a diff of two workspace files. The README states the Xylo desktop transport and attachment controls are still pending: **nothing in this milestone attaches IDE content to a Xylo prompt**. Clients (including any future Xylo transport) find the bridge through a discovery record. It is independent of the Rust workspace; see [desktop.md](desktop.md) and [services.md](services.md) for the Xylo side.

## Module map

| Path under `ide/vscode` | Purpose |
| --- | --- |
| `src/extension.ts` | `activate`/`deactivate`, workspace scoping (`scoped`), the four tool adapters |
| `src/server.ts` | `privateDirectory`, `startBridge`: HTTP upgrade, auth, MCP JSON-RPC, discovery record |
| `tests/server.test.ts` | Node test runner tests (two tests) |
| `package.json`, `tsconfig.json` | Metadata, scripts, `tsc` config (ES2022, Node16, strict, `outDir dist`) |

Dependencies: runtime `ws` 8.22.1; dev `typescript` 7.0.2, `@types/node`, `@types/ws`, `@types/vscode`. Activation event `onStartupFinished`; entry `./dist/src/extension.js`.

## Lifecycle (`extension.ts`)

`activate` registers `onDidChangeWorkspaceFolders` and `onDidGrantWorkspaceTrust`, both calling `restart`, which serializes on a promise chain: stop any current bridge, then return without starting if deactivated, the workspace is untrusted or no `file:` folders exist. Otherwise it resolves the real home directory, checks `~/.xylo` with `privateDirectory(..., false)` and starts the bridge in `~/.xylo/ide`. A failure shows a warning asking the user to check that both directories are private and owned by them. `deactivate` waits for pending starts and stops the bridge.

## Discovery record

Created by `startBridge` as `~/.xylo/ide/<pid>-<16 hex>.json`, mode `0600`, flag `wx` (never overwrites). JSON fields: `version: 1`, `pid`, `name: "VS Code"`, `endpoint` (`ws://127.0.0.1:<ephemeral port>/mcp`), `token` (32 random bytes, hex) and `workspaces` (real paths). The record is removed on `stop`. If writing fails, the bridge stops and the error propagates.

`privateDirectory` checks that the parent is a real directory and not a symlink, creates the directory with mode `0700`, then requires it to be a non-symlink directory owned by the current user (non-Windows) with no group/other access (`0o077` for `~/.xylo/ide`; only `0o022` for `~/.xylo`).

## MCP protocol

Transport: WebSocket at `/mcp`, listening on `127.0.0.1` with port 0. Negotiated protocol versions: `2025-11-25` (default), `2025-06-18`, `2025-03-26`; an unknown requested version gets `2025-11-25`. Server info `Xylo VS Code` 0.1.0, capabilities `{tools: {}}`.

Flow: `initialize`, then client notification `notifications/initialized`; before that only `initialize` and `ping` work (others get error `-32002`). Supported methods: `tools/list`, `tools/call`, `ping`; unknown methods return `-32601`; unknown tool or non-object arguments `-32602`. `tools/list` annotates every tool `readOnlyHint` (false only for `openDiff`), `destructiveHint: false`, `openWorldHint: false`.

## Tools

All locations are **one-based**. Every path must resolve (via `realpath`) inside a `file:` workspace folder of a trusted workspace, otherwise it is excluded or refused.

| Tool | Input | Result |
| --- | --- | --- |
| `getCurrentSelection` | none | `{selection: null}` without an active editor; else `path`, `language`, `start`/`end` (`line`, `character`), `text` (first 32 768 chars), `truncated` |
| `getOpenEditors` | none | `{editors: [{path, language, dirty}]}` from at most the first 100 documents; external and virtual documents omitted |
| `getDiagnostics` | none | `{diagnostics: [{path, line, character, severity, message (1000 chars), source}], truncated}`; at most 200 items |
| `openDiff` | `left`, `right`: absolute paths | `{opened: true}` after `vscode.diff` titled "Xylo diff" in preview mode |

## Limits and errors

- WebSocket `maxPayload` 65 536 bytes, `perMessageDeflate` off; at most **8** concurrent clients; at most **32** pending requests per connection (binary frames or overflow close with 1008); requests per connection are processed sequentially through a queue.
- Invalid JSON closes with 1007; a non-JSON-RPC 2.0 message or invalid id closes with 1008.
- A tool result over 262 144 bytes becomes an error. **Tool failures return `isError: true` with the generic text** "IDE request could not be completed within this workspace."; internal details are not disclosed.
- Any other HTTP request (non-upgrade) gets 404.

## Security

- **Bearer token auth**: the `Authorization: Bearer <token>` header is compared with `timingSafeEqual`; the token is never in the URL and is not logged.
- Upgrades are refused with 401 if the token is wrong, an `Origin` header is present (browser requests), the path is not `/mcp`, or 8 clients are connected.
- Listener binds loopback only. Discovery directories must be private and not symlinks; the record is `0600`.
- Starts only for trusted workspaces with `file:` folders; paths are `realpath`-resolved and confined to workspace roots (symlink escapes refused). Selection text is bounded.
- Only `openDiff` has a visible side effect (opens a diff editor); no tool edits files.

## Build and test (not run)

From `ide/vscode`: `npm ci`, `npm test` (`npm run build && node --test dist/tests/*.test.js`), `npm run build` (`tsc`). Tests: `private discovery authenticates MCP and returns selection without logging credentials` and `discovery refuses a symlinked directory`. To try it: `code --extensionDevelopmentPath=/absolute/path/to/xylo/ide/vscode /path/to/project`.

## Related docs

[desktop.md](desktop.md), [services.md](services.md), [cli.md](cli.md), [sdk.md](sdk.md), `ide/vscode/README.md`.
