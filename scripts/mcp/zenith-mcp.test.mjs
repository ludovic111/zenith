import assert from "node:assert/strict";
import { copyFile, mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const body = (result) => result.content.filter((c) => c.type === "text").map((c) => c.text).join("\n");

// Only synthetic data and a temporary HTTP server: never touch the live agent or its token.
async function fixture(t, respond) {
  const root = await mkdtemp(path.join(os.tmpdir(), "zenith-mcp-test-"));
  const server = http.createServer(respond);
  let client;
  t.after(async () => {
    await client?.close();
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    await rm(root, { recursive: true, force: true });
  });
  await mkdir(path.join(root, "scripts/mcp"), { recursive: true });
  await mkdir(path.join(root, "context"));
  await copyFile(path.join(ROOT, "scripts/mcp/zenith-mcp.mjs"), path.join(root, "scripts/mcp/zenith-mcp.mjs"));
  await symlink(path.join(ROOT, "node_modules"), path.join(root, "node_modules"), "dir");
  await writeFile(path.join(root, "context/brief.md"), "# Saved brief\n\nGenerated: 2026-01-01\n");
  const config = path.join(root, "config.json");
  await writeFile(config, JSON.stringify({ locale: "en", projects: [] }));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${server.address().port}`;
  client = new Client({ name: "zenith-test", version: "1.0.0" });
  await client.connect(new StdioClientTransport({
    command: process.execPath,
    args: [path.join(root, "scripts/mcp/zenith-mcp.mjs")],
    env: { ZENITH_CONFIG: config, ZENITH_URL: url },
    stderr: "pipe",
  }));
  return { client, server };
}

test("tool and resource reads use the live context when available", async (t) => {
  const { client } = await fixture(t, (req, res) => {
    assert.equal(req.url, "/api/context/brief");
    res.end("# Live brief");
  });
  const result = await client.callTool({ name: "zenith_brief", arguments: {} });
  assert.equal(result.isError, undefined);
  assert.equal(body(result), "# Live brief");
  const resource = await client.readResource({ uri: "zenith://brief" });
  assert.equal(resource.contents[0].text, "# Live brief");
});

test("a stopped server falls back to the saved context for tools and resources", async (t) => {
  const { client, server } = await fixture(t, (_req, res) => res.end());
  await new Promise((resolve) => server.close(resolve));
  for (const result of [
    body(await client.callTool({ name: "zenith_brief", arguments: {} })),
    (await client.readResource({ uri: "zenith://brief" })).contents[0].text,
  ]) {
    assert.match(result, /Saved brief/);
    assert.match(result, /Generated: 2026-01-01/);
    assert.match(result, /copy from disk/);
  }
  const failed = await client.callTool({ name: "zenith_now", arguments: {} });
  assert.equal(failed.isError, true);
  assert.match(body(failed), /not answering/);
});

test("a stalled response body falls back well before the MCP tool deadline", async (t) => {
  const { client } = await fixture(t, (_req, res) => {
    res.writeHead(200, { "content-type": "text/markdown" });
    res.write("unfinished live context");
  });
  const started = Date.now();
  const result = await client.callTool({ name: "zenith_brief", arguments: {} });
  assert.ok(Date.now() - started < 8_000, "disk fallback must not wait for the 60-second tool deadline");
  assert.match(body(result), /Saved brief/);
  assert.match(body(result), /copy from disk/);
  assert.doesNotMatch(body(result), /unfinished live context/);
});

test("HTTP failures are MCP errors for reads and actions, and actions are not retried", async (t) => {
  const requests = [];
  const { client } = await fixture(t, (req, res) => {
    requests.push([req.method, req.url]);
    res.writeHead(403, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: "Forbidden" }));
  });
  for (const [name, args] of [["zenith_now", {}], ["zenith_done", { id: "mail:example" }]]) {
    const result = await client.callTool({ name, arguments: args });
    assert.equal(result.isError, true);
    assert.match(body(result), /Failed: Forbidden/);
  }
  assert.deepEqual(requests, [["GET", "/api/now"], ["POST", "/api/now"]]);
});
