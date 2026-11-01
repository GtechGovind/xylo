/** VS Code selection, editor, diagnostics and diff adapters for Xylo. */
import * as vscode from "vscode";
import { homedir } from "node:os";
import { join, relative, isAbsolute } from "node:path";
import { realpath } from "node:fs/promises";
import { Bridge, BridgeTool, privateDirectory, startBridge } from "./server.js";

let bridge: Bridge | undefined;
let starting = Promise.resolve();
let stopped = false;
const emptySchema = {
  type: "object",
  properties: {},
  additionalProperties: false,
};

/** A bridge can only disclose local files beneath its trusted workspace roots. */
async function scoped(uri: vscode.Uri): Promise<string> {
  if (!vscode.workspace.isTrusted || uri.scheme !== "file")
    throw new Error("Select a trusted local workspace");
  const path = await realpath(uri.fsPath);
  const roots = await Promise.all(
    (vscode.workspace.workspaceFolders ?? [])
      .filter((folder) => folder.uri.scheme === "file")
      .map((folder) => realpath(folder.uri.fsPath)),
  );
  if (
    !roots.some((root) => {
      const child = relative(root, path);
      return (
        child !== ".." &&
        !child.startsWith(`..${process.platform === "win32" ? "\\" : "/"}`) &&
        !isAbsolute(child)
      );
    })
  )
    throw new Error("File is outside this workspace");
  return path;
}
function adapters(): BridgeTool[] {
  return [
    {
      name: "getCurrentSelection",
      description: "Read the current selected text in the trusted workspace.",
      inputSchema: emptySchema,
      call: async () => {
        const editor = vscode.window.activeTextEditor;
        if (!editor) return { selection: null };
        const path = await scoped(editor.document.uri);
        const text = editor.document.getText(editor.selection);
        return {
          path,
          language: editor.document.languageId,
          start: {
            line: editor.selection.start.line + 1,
            character: editor.selection.start.character + 1,
          },
          end: {
            line: editor.selection.end.line + 1,
            character: editor.selection.end.character + 1,
          },
          text: text.slice(0, 32768),
          truncated: text.length > 32768,
        };
      },
    },
    {
      name: "getOpenEditors",
      description: "List open local editors in the trusted workspace.",
      inputSchema: emptySchema,
      call: async () => {
        const editors = [];
        for (const document of vscode.workspace.textDocuments.slice(0, 100)) {
          try {
            editors.push({
              path: await scoped(document.uri),
              language: document.languageId,
              dirty: document.isDirty,
            });
          } catch {
            /* Exclude external and virtual documents. */
          }
        }
        return { editors };
      },
    },
    {
      name: "getDiagnostics",
      description: "Read current workspace diagnostics from the IDE.",
      inputSchema: emptySchema,
      call: async () => {
        const diagnostics = [];
        for (const [uri, items] of vscode.languages.getDiagnostics()) {
          let path: string;
          try {
            path = await scoped(uri);
          } catch {
            continue;
          }
          for (const item of items) {
            if (diagnostics.length >= 200)
              return { diagnostics, truncated: true };
            diagnostics.push({
              path,
              line: item.range.start.line + 1,
              character: item.range.start.character + 1,
              severity: item.severity,
              message: item.message.slice(0, 1000),
              source: item.source,
            });
          }
        }
        return { diagnostics, truncated: false };
      },
    },
    {
      name: "openDiff",
      description:
        "Open a comparison of two existing files inside this workspace.",
      inputSchema: {
        type: "object",
        properties: { left: { type: "string" }, right: { type: "string" } },
        required: ["left", "right"],
        additionalProperties: false,
      },
      call: async (args) => {
        if (
          typeof args.left !== "string" ||
          typeof args.right !== "string" ||
          !isAbsolute(args.left) ||
          !isAbsolute(args.right)
        )
          throw new Error("Two absolute workspace paths are required");
        const left = vscode.Uri.file(await scoped(vscode.Uri.file(args.left)));
        const right = vscode.Uri.file(
          await scoped(vscode.Uri.file(args.right)),
        );
        await vscode.commands.executeCommand(
          "vscode.diff",
          left,
          right,
          "Xylo diff",
          { preview: true },
        );
        return { opened: true };
      },
    },
  ];
}
export function activate(context: vscode.ExtensionContext): void {
  const restart = () => {
    starting = starting
      .then(async () => {
        await bridge?.stop();
        bridge = undefined;
        if (stopped || !vscode.workspace.isTrusted) return;
        const roots = (vscode.workspace.workspaceFolders ?? [])
          .filter((folder) => folder.uri.scheme === "file")
          .map((folder) => folder.uri.fsPath);
        if (!roots.length) return;
        const home = await realpath(homedir());
        await privateDirectory(join(home, ".xylo"), false);
        bridge = await startBridge(
          join(home, ".xylo", "ide"),
          roots,
          adapters(),
        );
      })
      .catch(() => {
        void vscode.window.showWarningMessage(
          "Xylo could not start the IDE bridge. Check that ~/.xylo and ~/.xylo/ide are private directories owned by you.",
        );
      });
  };
  stopped = false;
  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders(restart),
    vscode.workspace.onDidGrantWorkspaceTrust(restart),
  );
  restart();
}
export async function deactivate(): Promise<void> {
  stopped = true;
  await starting;
  await bridge?.stop();
  bridge = undefined;
}
