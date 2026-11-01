import assert from "node:assert/strict";
import { mkdtemp, readFile, stat, rm, mkdir, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { once } from "node:events";
import { test } from "node:test";
import WebSocket from "ws";
import { startBridge } from "../src/server.js";

test("private discovery authenticates MCP and returns selection without logging credentials", async () => {
  const directory = await mkdtemp(join(tmpdir(), "xylo-ide-"));
  const bridge = await startBridge(
    join(directory, "ide"),
    [directory],
    [
      {
        name: "getCurrentSelection",
        description: "Selection",
        inputSchema: { type: "object" },
        call: async () => ({
          path: join(directory, "file.ts"),
          text: "selected text",
        }),
      },
    ],
  );
  try {
    const record = JSON.parse(await readFile(bridge.lockfile, "utf8"));
    assert.match(record.endpoint, /^ws:\/\/127\.0\.0\.1:\d+\/mcp$/);
    assert.match(record.token, /^[a-f0-9]{64}$/);
    if (process.platform !== "win32")
      assert.equal((await stat(bridge.lockfile)).mode & 0o777, 0o600);
    const reject = async (headers: Record<string, string>) => {
      const client = new WebSocket(record.endpoint, { headers });
      const [error] = await once(client, "error");
      assert.match(String(error), /401|403/);
    };
    await reject({});
    await reject({
      Authorization: `Bearer ${record.token}`,
      Origin: "https://attacker.invalid",
    });
    const client = new WebSocket(record.endpoint, {
      headers: { Authorization: `Bearer ${record.token}` },
    });
    await once(client, "open");
    const rpc = async (id: number, method: string, params: object = {}) => {
      const reply = once(client, "message");
      client.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
      return JSON.parse(String((await reply)[0]));
    };
    assert.equal((await rpc(1, "tools/list")).error.code, -32002);
    assert.equal(
      (
        await rpc(2, "initialize", {
          protocolVersion: "2025-11-25",
          capabilities: {},
          clientInfo: { name: "fixture", version: "1" },
        })
      ).result.protocolVersion,
      "2025-11-25",
    );
    client.send(
      JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }),
    );
    assert.equal(
      (await rpc(3, "tools/list")).result.tools[0].name,
      "getCurrentSelection",
    );
    const selection = await rpc(4, "tools/call", {
      name: "getCurrentSelection",
      arguments: {},
    });
    assert.equal(
      JSON.parse(selection.result.content[0].text).text,
      "selected text",
    );
    client.close();
    await once(client, "close");
  } finally {
    await bridge.stop();
    await assert.rejects(stat(bridge.lockfile));
    await rm(directory, { recursive: true, force: true });
  }
});

test("discovery refuses a symlinked directory", async () => {
  const directory = await mkdtemp(join(tmpdir(), "xylo-ide-"));
  try {
    await mkdir(join(directory, "target"));
    await symlink(join(directory, "target"), join(directory, "ide"));
    await assert.rejects(
      startBridge(join(directory, "ide"), [], []),
      /symbolic|private/i,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
