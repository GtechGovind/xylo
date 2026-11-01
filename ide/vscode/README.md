# Xylo IDE Bridge

This local VS Code extension lets Xylo attach the current selection and diagnostics from a trusted workspace. It also exposes open editor metadata and opens comparisons of two workspace files.

Run `npm ci`, `npm test`, and `npm run build` from this directory. Start VS Code with `code --extensionDevelopmentPath=/absolute/path/to/xylo/ide/vscode /path/to/project` to load the extension in a development window. Open the same project in Xylo and use its IDE attachment control.

The extension starts only for trusted, local workspaces. Each window writes a private discovery record under `~/.xylo/ide/` and listens on an ephemeral loopback port. Xylo uses the record's random bearer token to connect; browser-origin requests are rejected. The record is removed when the extension stops. Selection text is bounded, and files outside the workspace are excluded. No token is placed in the URL or logs.

The bridge implements MCP initialization and `tools/list` / `tools/call` over WebSocket, negotiating protocol versions 2025-11-25, 2025-06-18 or 2025-03-26. The four tools are `getCurrentSelection`, `getOpenEditors`, `getDiagnostics` and `openDiff`. Locations use one-based lines and characters. `openDiff` accepts absolute `left` and `right` paths, both confined to the trusted workspace.

API references: [VS Code extension API](https://code.visualstudio.com/api/references/vscode-api) and [MCP connection lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle).
