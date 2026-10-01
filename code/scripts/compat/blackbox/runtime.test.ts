/**
 * The server runtime around the feature crates, black-box (plan §6.14–6.18): lifecycle events,
 * the ServerConfig assembly, keybindings, the probe, the cloud and device stubs, and the
 * `project add|remove|rename` CLI, online (through the running server) and offline (engine in
 * the CLI process).
 */
import * as NodeTest from "node:test";
import * as NodeAssert from "node:assert/strict";
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import { RawRpcClient, http } from "../lib/client.ts";
import { startServer } from "../lib/serverUnderTest.ts";
import { SYNTHETIC_SETTINGS } from "../lib/fixtures.ts";
import { useServer } from "./harness.ts";

type LifecycleEvent = {
  version: number;
  sequence: number;
  type: string;
  payload: { environment: { environmentId: string }; cwd?: string; projectName?: string };
};

NodeTest.describe("runtime: lifecycle and config", () => {
  const ctx = useServer();

  // serverRuntimeStartup.ts: welcome, then ready, both carrying the descriptor.
  NodeTest.test("subscribeServerLifecycle replays welcome then ready", async () => {
    const descriptor = await http<{ environmentId: string }>(
      `${ctx.httpUrl}/.well-known/t3/environment`,
    );
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    const lifecycle = rpc.stream("subscribeServerLifecycle", {});
    const values = (await lifecycle.waitForValues(2)) as Array<LifecycleEvent>;
    NodeAssert.deepEqual(
      values.map((v) => v.type),
      ["welcome", "ready"],
    );
    NodeAssert.ok(values[0]!.sequence < values[1]!.sequence);
    for (const event of values) {
      NodeAssert.equal(event.version, 1);
      NodeAssert.equal(event.payload.environment.environmentId, descriptor.body.environmentId);
    }
    NodeAssert.equal(typeof values[0]!.payload.cwd, "string");
    NodeAssert.equal(typeof values[0]!.payload.projectName, "string");
    await rpc.close();
  });

  NodeTest.test("server.getConfig assembles environment, auth, providers and settings", async () => {
    const descriptor = await http<Record<string, unknown>>(
      `${ctx.httpUrl}/.well-known/t3/environment`,
    );
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    const config = await rpc.callOk<Record<string, unknown>>("server.getConfig", {});
    NodeAssert.deepEqual(config.environment, descriptor.body);
    NodeAssert.equal((config.auth as { policy: string }).policy, "loopback-browser");
    NodeAssert.ok(Array.isArray(config.providers));
    NodeAssert.ok(Array.isArray(config.availableEditors));
    NodeAssert.ok(Array.isArray(config.keybindings));
    NodeAssert.equal(typeof config.settings, "object");
    NodeAssert.equal(
      config.keybindingsConfigPath,
      NodePath.join(ctx.server.homeDir, "userdata/keybindings.json"),
    );
    await rpc.close();
  });

  NodeTest.test("server.probe answers an empty object", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    NodeAssert.deepEqual(await rpc.callOk("server.probe", {}), {});
    await rpc.close();
  });

  // ws.ts server.upsertKeybinding / removeKeybinding, and keybindingsUpdated on the config stream.
  NodeTest.test("keybindings: upsert and remove, pushed to config subscribers", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    const config = rpc.stream("subscribeServerConfig", {});
    await config.waitForValues(1);
    const rule = { key: "mod+shift+9", command: "terminal.toggle" };
    const upserted = await rpc.callOk<{ keybindings: Array<{ command: string }> }>(
      "server.upsertKeybinding",
      rule,
    );
    NodeAssert.ok(upserted.keybindings.some((k) => k.command === "terminal.toggle"));
    const onDisk = NodeFS.readFileSync(
      NodePath.join(ctx.server.homeDir, "userdata/keybindings.json"),
      "utf8",
    );
    NodeAssert.match(onDisk, /mod\+shift\+9/);
    let pushed: { type: string } | undefined;
    for (let attempt = 0; attempt < 20 && !pushed; attempt++) {
      pushed = (config.values as Array<{ type: string }>).find(
        (v) => v.type === "keybindingsUpdated",
      );
      if (!pushed) await new Promise((r) => setTimeout(r, 100));
    }
    NodeAssert.ok(pushed, JSON.stringify(config.values.map((v) => (v as { type: string }).type)));
    const removed = await rpc.callOk<{ keybindings: Array<unknown> }>(
      "server.removeKeybinding",
      rule,
    );
    NodeAssert.ok(Array.isArray(removed.keybindings));
    NodeAssert.doesNotMatch(
      NodeFS.readFileSync(NodePath.join(ctx.server.homeDir, "userdata/keybindings.json"), "utf8"),
      /mod\+shift\+9/,
    );
    await rpc.close();
  });

  NodeTest.test("the relay client status and the device state answer", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    const relay = await rpc.callOk<{ status: string; version: string }>(
      "cloud.getRelayClientStatus",
      {},
    );
    NodeAssert.ok(["missing", "available", "unsupported"].includes(relay.status));
    NodeAssert.equal(typeof relay.version, "string");
    const devices = rpc.stream("subscribeDeviceState", {});
    const [state] = (await devices.waitForValues(1)) as Array<{
      hubBasePath: string;
      devices: Array<unknown>;
      sessions: Array<unknown>;
    }>;
    NodeAssert.equal(state!.hubBasePath, "/api/device-hub");
    NodeAssert.ok(Array.isArray(state!.devices));
    NodeAssert.ok(Array.isArray(state!.sessions));
    await rpc.close();
  });

  // Plan §6.14: the cloud link state may be probed; an unlinked environment says so.
  NodeTest.test("GET /api/connect/link-state reports an unlinked environment", async () => {
    const unauthenticated = await http(`${ctx.httpUrl}/api/connect/link-state`);
    NodeAssert.equal(unauthenticated.status, 401);
    const res = await http<{ linked: boolean; publishAgentActivity: boolean }>(
      `${ctx.httpUrl}/api/connect/link-state`,
      { headers: { cookie: await ctx.adminCookie() } },
    );
    NodeAssert.equal(res.status, 200, res.text);
    NodeAssert.equal(res.body.linked, false);
    NodeAssert.equal(res.body.publishAgentActivity, false);
  });
});

const ADDED = /^Added project (\S+) \((.+)\) at (.+)\.$/m;

NodeTest.describe("runtime: project CLI through the running server", () => {
  const ctx = useServer();

  NodeTest.test("project add, rename and remove go through the live server", async () => {
    const root = NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-cli-project-"));
    try {
      const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
      const shell = rpc.stream("orchestration.subscribeShell", { requestCompletionMarker: true });
      await shell.waitForValues(2);

      const added = await ctx.server.cli(["project", "add", root, "--title", "Sample project"]);
      NodeAssert.equal(added.code, 0, added.stderr);
      const match = ADDED.exec(added.stdout);
      NodeAssert.ok(match, added.stdout);
      const [, projectId, title, workspaceRoot] = match;
      NodeAssert.equal(title, "Sample project");
      NodeAssert.equal(NodeFS.realpathSync(workspaceRoot!), NodeFS.realpathSync(root));
      // The live server published it: the CLI went through its API, not around it.
      await shell.waitForValues(3);
      NodeAssert.ok(
        shell.values.slice(2).some((v) => (v as { kind: string }).kind === "project-upserted"),
      );

      const again = await ctx.server.cli(["project", "add", root]);
      NodeAssert.notEqual(again.code, 0);
      NodeAssert.match(again.stdout + again.stderr, /already exists/i);

      const renamed = await ctx.server.cli(["project", "rename", projectId!, "Renamed sample"]);
      NodeAssert.equal(renamed.code, 0, renamed.stderr);
      NodeAssert.match(renamed.stdout, new RegExp(`^Renamed project ${projectId} to Renamed sample\\.$`, "m"));
      const same = await ctx.server.cli(["project", "rename", workspaceRoot!, "Renamed sample"]);
      NodeAssert.match(same.stdout, /is already named Renamed sample\./);

      const removed = await ctx.server.cli(["project", "remove", projectId!]);
      NodeAssert.equal(removed.code, 0, removed.stderr);
      NodeAssert.match(removed.stdout, new RegExp(`^Removed project ${projectId} \\(Renamed sample\\)\\.$`, "m"));
      const missing = await ctx.server.cli(["project", "remove", projectId!]);
      NodeAssert.notEqual(missing.code, 0);
      NodeAssert.match(missing.stdout + missing.stderr, /No active project found/);

      const snapshot = await http<{ projects: Array<{ id: string }> }>(
        `${ctx.httpUrl}/api/orchestration/shell`,
        { headers: { cookie: await ctx.adminCookie() } },
      );
      NodeAssert.ok(!snapshot.body.projects.some((p) => p.id === projectId));
      await rpc.close();
    } finally {
      NodeFS.rmSync(root, { recursive: true, force: true });
    }
  });
});

NodeTest.describe("runtime: project CLI offline", () => {
  NodeTest.test("project add without a server writes through the engine", async () => {
    const home = NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-cli-offline-"));
    const root = NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-cli-root-"));
    try {
      const first = await startServer({ homeDir: home, isolateHome: true, settings: SYNTHETIC_SETTINGS });
      await first.stop();
      NodeAssert.ok(!NodeFS.existsSync(NodePath.join(home, "userdata/server-runtime.json")));

      const added = await first.cli(["project", "add", root]);
      NodeAssert.equal(added.code, 0, added.stderr);
      const match = ADDED.exec(added.stdout);
      NodeAssert.ok(match, added.stdout);
      NodeAssert.equal(match[2], NodePath.basename(root));

      // A stale runtime file (dead server) is ignored and removed.
      NodeFS.writeFileSync(
        NodePath.join(home, "userdata/server-runtime.json"),
        `${JSON.stringify({ version: 1, pid: 999999, host: "127.0.0.1", port: 9, origin: "http://127.0.0.1:9", startedAt: new Date().toISOString() })}\n`,
      );
      const renamed = await first.cli(["project", "rename", match[1]!, "Offline sample"]);
      NodeAssert.equal(renamed.code, 0, renamed.stderr);
      NodeAssert.ok(!NodeFS.existsSync(NodePath.join(home, "userdata/server-runtime.json")));

      const second = await startServer({ homeDir: home, isolateHome: true, settings: SYNTHETIC_SETTINGS });
      try {
        const cookieRes = await (await import("../lib/client.ts")).bootstrapBrowserSession(
          second.httpUrl,
          second.bootstrapCredential,
        );
        const shell = await http<{ projects: Array<{ id: string; title: string }> }>(
          `${second.httpUrl}/api/orchestration/shell`,
          { headers: { cookie: cookieRes.cookie! } },
        );
        NodeAssert.ok(
          shell.body.projects.some((p) => p.id === match[1] && p.title === "Offline sample"),
          JSON.stringify(shell.body.projects),
        );
      } finally {
        await second.stop();
      }
    } finally {
      NodeFS.rmSync(home, { recursive: true, force: true });
      NodeFS.rmSync(root, { recursive: true, force: true });
    }
  });
});
