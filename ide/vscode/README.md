# Xylo IDE Bridge

This development extension exposes the current selection, diagnostics and open editor metadata from a trusted VS Code workspace through a local MCP server. It can also open comparisons of two workspace files. The Xylo desktop transport and attachment controls are still pending; this milestone does not yet attach IDE content to a Xylo prompt.

Run `npm ci`, `npm test`, and `npm run build` from this directory. Start VS Code with `code --extensionDevelopmentPath=/absolute/path/to/xylo/ide/vscode /path/to/project` to load the extension in a development window. The extension can be exercised independently with the authenticated MCP protocol described below.

The extension starts only for trusted, local workspaces. Each window writes a private discovery record under `~/.xylo/ide/` and listens on an ephemeral loopback port. Clients must use the record's random bearer token to connect; browser-origin requests are rejected. The record is removed when the extension stops. Selection text is bounded, and files outside the workspace are excluded. No token is placed in the URL or logs.

The bridge implements MCP initialization and `tools/list` / `tools/call` over WebSocket, negotiating protocol versions 2025-11-25, 2025-06-18 or 2025-03-26. The four tools are `getCurrentSelection`, `getOpenEditors`, `getDiagnostics` and `openDiff`. Locations use one-based lines and characters. `openDiff` accepts absolute `left` and `right` paths, both confined to the trusted workspace.

API references: [VS Code extension API](https://code.visualstudio.com/api/references/vscode-api) and [MCP connection lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle).
