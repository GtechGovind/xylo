/** Authenticated local MCP bridge and private discovery record. */
import { randomBytes, timingSafeEqual } from "node:crypto";
import { createServer } from "node:http";
import { lstat, mkdir, realpath, unlink, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { once } from "node:events";
import WebSocket, { WebSocketServer } from "ws";

export type BridgeTool = {
  name: string;
  description: string;
  inputSchema: object;
  call: (args: Record<string, unknown>) => Promise<unknown>;
};
export type Bridge = { lockfile: string; stop: () => Promise<void> };
const protocols = ["2025-11-25", "2025-06-18", "2025-03-26"];
const object = (value: unknown): value is Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value);

/** The caller supplies a directory below its own home, never a project path. */
export async function privateDirectory(
  directory: string,
  privateContents = true,
): Promise<void> {
  // Check the direct parent before creating anything through it. Repeating this
  // for ~/.xylo and ~/.xylo/ide protects both discovery components.
  const parent = await lstat(dirname(directory));
  if (!parent.isDirectory() || parent.isSymbolicLink())
    throw new Error("Discovery needs a private directory, not a symbolic link");
  await mkdir(directory, { mode: 0o700 }).catch(
    (error: NodeJS.ErrnoException) => {
      if (error.code !== "EEXIST") throw error;
    },
  );
  const info = await lstat(directory);
  if (
    !info.isDirectory() ||
    info.isSymbolicLink() ||
    (process.platform !== "win32" &&
      ((info.mode & (privateContents ? 0o077 : 0o022)) !== 0 ||
        info.uid !== process.getuid?.()))
  ) {
    throw new Error(
      "IDE discovery directory must be private and owned by this user",
    );
  }
}

/** Starts a bounded WebSocket transport. The bearer token never enters a URL. */
export async function startBridge(
  directory: string,
  workspace: string[],
  tools: BridgeTool[],
): Promise<Bridge> {
  await privateDirectory(directory);
  const token = randomBytes(32).toString("hex");
  const expected = Buffer.from(`Bearer ${token}`);
  const http = createServer((_, response) => {
    response.writeHead(404);
    response.end();
  });
  const sockets = new WebSocketServer({
    noServer: true,
    maxPayload: 65536,
    perMessageDeflate: false,
  });
  http.on("upgrade", (request, socket, head) => {
    const supplied = Buffer.from(request.headers.authorization ?? "");
    const authenticated =
      supplied.length === expected.length &&
      timingSafeEqual(supplied, expected);
    if (
      !authenticated ||
      request.headers.origin !== undefined ||
      request.url !== "/mcp" ||
      sockets.clients.size >= 8
    ) {
      socket.end("HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n");
      return;
    }
    sockets.handleUpgrade(request, socket, head, (client) =>
      sockets.emit("connection", client),
    );
  });
  sockets.on("connection", (client) => {
    let negotiated = false;
    let ready = false;
    let pending = 0;
    let queue = Promise.resolve();
    client.on("error", () => {});
    client.on("message", (bytes, binary) => {
      if (binary || ++pending > 32) {
        client.close(1008, "Too many requests");
        return;
      }
      queue = queue
        .then(async () => {
          let message: unknown;
          try {
            message = JSON.parse(bytes.toString());
          } catch {
            client.close(1007, "Invalid JSON");
            return;
          }
          if (
            !object(message) ||
            message.jsonrpc !== "2.0" ||
            typeof message.method !== "string"
          ) {
            client.close(1008, "Invalid request");
            return;
          }
          const { id, method } = message;
          if (id === undefined) {
            if (method === "notifications/initialized" && negotiated)
              ready = true;
            return;
          }
          if (typeof id !== "string" && typeof id !== "number") {
            client.close(1008, "Invalid request id");
            return;
          }
          const reply = (result: unknown) => {
            if (client.readyState === WebSocket.OPEN)
              client.send(JSON.stringify({ jsonrpc: "2.0", id, result }));
          };
          const fail = (code: number, description: string) => {
            if (client.readyState === WebSocket.OPEN)
              client.send(
                JSON.stringify({
                  jsonrpc: "2.0",
                  id,
                  error: { code, message: description },
                }),
              );
          };
          const params = object(message.params) ? message.params : {};
          if (method === "initialize" && !negotiated) {
            negotiated = true;
            const protocolVersion = protocols.includes(
              String(params.protocolVersion),
            )
              ? params.protocolVersion
              : protocols[0];
            reply({
              protocolVersion,
              capabilities: { tools: {} },
              serverInfo: { name: "Xylo VS Code", version: "0.1.0" },
            });
            return;
          }
          if (method === "ping") {
            reply({});
            return;
          }
          if (!ready) {
            fail(-32002, "Initialize the IDE connection first");
            return;
          }
          if (method === "tools/list") {
            reply({
              tools: tools.map(({ name, description, inputSchema }) => ({
                name,
                description,
                inputSchema,
                annotations: {
                  readOnlyHint: name !== "openDiff",
                  destructiveHint: false,
                  openWorldHint: false,
                },
              })),
            });
            return;
          }
          if (method !== "tools/call") {
            fail(-32601, "Method not found");
            return;
          }
          const tool = tools.find((tool) => tool.name === params.name);
          if (!tool || !object(params.arguments ?? {})) {
            fail(-32602, "Unknown tool or invalid arguments");
            return;
          }
          try {
            const output = JSON.stringify(
              await tool.call(
                (params.arguments ?? {}) as Record<string, unknown>,
              ),
            );
            if (Buffer.byteLength(output) > 262144)
              throw new Error("Result too large");
            reply({ content: [{ type: "text", text: output }] });
          } catch {
            reply({
              isError: true,
              content: [
                {
                  type: "text",
                  text: "IDE request could not be completed within this workspace.",
                },
              ],
            });
          }
        })
        .catch(() => client.close(1011, "Request failed"))
        .finally(() => {
          pending--;
        });
    });
  });
  http.listen(0, "127.0.0.1");
  await once(http, "listening");
  const address = http.address();
  if (!address || typeof address === "string")
    throw new Error("IDE listener is unavailable");
  const lockfile = join(
    directory,
    `${process.pid}-${randomBytes(8).toString("hex")}.json`,
  );
  const stop = async () => {
    for (const client of sockets.clients) client.terminate();
    await new Promise<void>((resolve) => sockets.close(() => resolve()));
    await new Promise<void>((resolve) => http.close(() => resolve()));
    await unlink(lockfile).catch(() => {});
  };
  try {
    const workspaces = await Promise.all(
      workspace.map((path) => realpath(path)),
    );
    await writeFile(
      lockfile,
      JSON.stringify({
        version: 1,
        pid: process.pid,
        name: "VS Code",
        endpoint: `ws://127.0.0.1:${address.port}/mcp`,
        token,
        workspaces,
      }),
      { mode: 0o600, flag: "wx" },
    );
  } catch (error) {
    await stop();
    throw error;
  }
  return { lockfile, stop };
}
