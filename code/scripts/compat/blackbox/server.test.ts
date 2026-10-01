/**
 * Server-level contracts the dashboard and the web client rely on, black-box: the environment
 * descriptor, zenith's embed.json, server-runtime.json, clean SIGTERM, the CLI output contract,
 * and orchestration over HTTP + WS on a real temp project.
 */
import * as NodeTest from "node:test";
import * as NodeAssert from "node:assert/strict";
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import * as NodeCrypto from "node:crypto";
import { RawRpcClient, bootstrapBrowserSession, http } from "../lib/client.ts";
import { startServer } from "../lib/serverUnderTest.ts";
import { useServer } from "./harness.ts";

NodeTest.describe("server: descriptor and embed", () => {
  const ctx = useServer({
    env: {
      ZENITH_CODE_PARENT_ORIGINS:
        "http://127.0.0.1:4747,http://localhost:9999/x,ftp://nope,http://127.0.0.1:4747",
    },
  });

  // server.test.ts:2162 (black-box: the id comes from <home>/userdata/environment-id)
  NodeTest.test("serves the public environment descriptor without requiring auth", async () => {
    const res = await http<{
      environmentId: string;
      label: string;
      platform: { os: string; arch: string };
      serverVersion: string;
      orchestrationProtocolVersion: number;
      capabilities: Record<string, unknown>;
    }>(`${ctx.httpUrl}/.well-known/t3/environment`);
    NodeAssert.equal(res.status, 200);
    const environmentId = NodeFS.readFileSync(
      NodePath.join(ctx.server.homeDir, "userdata/environment-id"),
      "utf8",
    ).trim();
    NodeAssert.equal(res.body.environmentId, environmentId);
    NodeAssert.equal(res.body.orchestrationProtocolVersion, 1);
    NodeAssert.match(res.body.serverVersion, /^\d+\.\d+\.\d+/);
    NodeAssert.equal(typeof res.body.capabilities, "object");
  });

  // Plan §2.7: entries kept only when they are an exact http(s) origin; duplicates removed.
  NodeTest.test("serves /zenith/embed.json publicly with no-store", async () => {
    const res = await http<{ parentOrigins: Array<string> }>(`${ctx.httpUrl}/zenith/embed.json`);
    NodeAssert.equal(res.status, 200);
    NodeAssert.equal(res.headers["cache-control"], "no-store");
    NodeAssert.deepEqual(res.body.parentOrigins, ["http://127.0.0.1:4747"]);
    NodeAssert.equal(res.headers["content-security-policy"], undefined);
  });

  NodeTest.test("writes userdata/server-runtime.json while running", async () => {
    const runtime = JSON.parse(
      NodeFS.readFileSync(
        NodePath.join(ctx.server.homeDir, "userdata/server-runtime.json"),
        "utf8",
      ),
    ) as Record<string, unknown>;
    NodeAssert.equal(runtime.version, 1);
    NodeAssert.equal(runtime.port, ctx.server.port);
    NodeAssert.equal(runtime.pid, ctx.server.pid);
    NodeAssert.equal(runtime.host, "127.0.0.1");
    NodeAssert.equal(runtime.origin, `http://127.0.0.1:${ctx.server.port}`);
    NodeAssert.equal(typeof runtime.startedAt, "string");
  });
});

NodeTest.describe("server: embed defaults", () => {
  const ctx = useServer();
  NodeTest.test("defaults parentOrigins to the dashboard's origins", async () => {
    const res = await http<{ parentOrigins: Array<string> }>(`${ctx.httpUrl}/zenith/embed.json`);
    NodeAssert.deepEqual(res.body.parentOrigins, [
      "http://127.0.0.1:4747",
      "http://127.0.0.1:4748",
    ]);
  });
});

NodeTest.describe("server: lifecycle", () => {
  NodeTest.test(
    "prints the banner, exits within 5 s on SIGTERM and removes server-runtime.json",
    async () => {
      const home = NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-lifecycle-"));
      try {
        const server = await startServer({ homeDir: home });
        const out = server.output();
        NodeAssert.match(out, /^(T3 Code|zenith) server is ready\.$/m);
        NodeAssert.match(
          out,
          new RegExp(`^Connection string: http://127\\.0\\.0\\.1:${server.port}$`, "m"),
        );
        NodeAssert.match(
          out,
          /^Pairing URL: http:\/\/127\.0\.0\.1:\d+\/pair#token=[23456789A-Z]{12}$/m,
        );
        const runtimeFile = NodePath.join(home, "userdata/server-runtime.json");
        NodeAssert.ok(NodeFS.existsSync(runtimeFile));
        const stopped = await server.stop();
        NodeAssert.ok(stopped.ms < 5000, `took ${stopped.ms} ms`);
        NodeAssert.equal(stopped.signal === "SIGKILL", false);
        NodeAssert.ok(
          !NodeFS.existsSync(runtimeFile),
          "server-runtime.json must be deleted on shutdown",
        );
        // Plan §3.2: the state survives a restart (same environment id).
        const environmentId = NodeFS.readFileSync(
          NodePath.join(home, "userdata/environment-id"),
          "utf8",
        ).trim();
        const again = await startServer({ homeDir: home });
        const descriptor = await http<{ environmentId: string }>(
          `${again.httpUrl}/.well-known/t3/environment`,
        );
        NodeAssert.equal(descriptor.body.environmentId, environmentId);
        await again.stop();
      } finally {
        NodeFS.rmSync(home, { recursive: true, force: true });
      }
    },
  );
});

NodeTest.describe("server: CLI contract (plan §6.17)", () => {
  const ctx = useServer();

  NodeTest.test("auth pairing create --json prints a usable credential", async () => {
    const result = await ctx.server.cli([
      "auth",
      "pairing",
      "create",
      "--admin",
      "--label",
      "cli",
      "--json",
    ]);
    NodeAssert.equal(result.code, 0, result.stderr);
    const body = JSON.parse(result.stdout.slice(result.stdout.indexOf("{"))) as {
      id: string;
      credential: string;
      label?: string;
      scopes: Array<string>;
      expiresAt: string;
    };
    NodeAssert.match(body.credential, /^[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{12}$/);
    NodeAssert.equal(body.label, "cli");
    NodeAssert.ok(body.scopes.includes("access:write"));
    NodeAssert.ok(!Number.isNaN(Date.parse(body.expiresAt)));
    const boot = await bootstrapBrowserSession(ctx.httpUrl, body.credential);
    NodeAssert.equal(boot.status, 200);
  });

  NodeTest.test("auth session issue --json prints a bearer token the server accepts", async () => {
    const result = await ctx.server.cli([
      "auth",
      "session",
      "issue",
      "--ttl",
      "1h",
      "--label",
      "bot",
      "--json",
    ]);
    NodeAssert.equal(result.code, 0, result.stderr);
    const body = JSON.parse(result.stdout.slice(result.stdout.indexOf("{"))) as {
      sessionId: string;
      token: string;
      method: string;
      scopes: Array<string>;
      client: { label?: string; deviceType: string };
    };
    NodeAssert.equal(body.method, "bearer-access-token");
    NodeAssert.equal(body.client.deviceType, "bot");
    const session = await http<{ authenticated: boolean }>(`${ctx.httpUrl}/api/auth/session`, {
      headers: { authorization: `Bearer ${body.token}` },
    });
    NodeAssert.equal(session.body.authenticated, true);
  });

  NodeTest.test("auth pairing revoke of an unknown id says so", async () => {
    const result = await ctx.server.cli(["auth", "pairing", "revoke", "does-not-exist"]);
    NodeAssert.match(
      result.stdout + result.stderr,
      /No active pairing credential found for does-not-exist\./,
    );
  });
});

NodeTest.describe("server: orchestration on a real temp project", () => {
  const ctx = useServer();

  NodeTest.test(
    "project.create + thread.create over HTTP dispatch show up in the shell (HTTP and WS)",
    async () => {
      const cookie = await ctx.adminCookie();
      const workspaceRoot = NodeFS.mkdtempSync(
        NodePath.join(NodeOS.tmpdir(), "zenith-code-project-"),
      );
      try {
        const projectId = NodeCrypto.randomUUID();
        const threadId = NodeCrypto.randomUUID();
        const now = new Date().toISOString();
        const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie });
        const shell = rpc.stream("orchestration.subscribeShell", { requestCompletionMarker: true });
        await shell.waitForValues(2);

        const create = await http<{ sequence: number }>(
          `${ctx.httpUrl}/api/orchestration/dispatch`,
          {
            method: "POST",
            headers: { cookie },
            json: {
              type: "project.create",
              commandId: NodeCrypto.randomUUID(),
              projectId,
              title: "Synthetic project",
              workspaceRoot,
              createdAt: now,
            },
          },
        );
        NodeAssert.equal(create.status, 200, create.text);
        NodeAssert.equal(typeof create.body.sequence, "number");

        const thread = await http<{ sequence: number }>(
          `${ctx.httpUrl}/api/orchestration/dispatch`,
          {
            method: "POST",
            headers: { cookie },
            json: {
              type: "thread.create",
              commandId: NodeCrypto.randomUUID(),
              threadId,
              projectId,
              title: "Synthetic thread",
              modelSelection: { instanceId: "codex", model: "gpt-5-codex" },
              runtimeMode: "approval-required",
              interactionMode: "default",
              branch: null,
              worktreePath: null,
              createdAt: now,
            },
          },
        );
        NodeAssert.equal(thread.status, 200, thread.text);
        NodeAssert.ok(thread.body.sequence > create.body.sequence);

        const snapshot = await http<{
          snapshotSequence: number;
          projects: Array<{ id: string; title: string }>;
          threads: Array<{ id: string; title: string }>;
        }>(`${ctx.httpUrl}/api/orchestration/shell`, { headers: { cookie } });
        NodeAssert.equal(snapshot.status, 200);
        NodeAssert.ok(
          snapshot.body.projects.some((p) => p.id === projectId && p.title === "Synthetic project"),
        );
        NodeAssert.ok(
          snapshot.body.threads.some((t) => t.id === threadId && t.title === "Synthetic thread"),
        );
        NodeAssert.ok(snapshot.body.snapshotSequence >= thread.body.sequence);

        const detail = await http<{ thread: { id: string } }>(
          `${ctx.httpUrl}/api/orchestration/threads/${encodeURIComponent(threadId)}`,
          { headers: { cookie } },
        );
        NodeAssert.equal(detail.status, 200, detail.text);
        NodeAssert.equal(detail.body.thread.id, threadId);

        // Live shell events, each with a sequence.
        await shell.waitForValues(4);
        const kinds = shell.values.slice(2).map((v) => (v as { kind: string }).kind);
        NodeAssert.ok(kinds.includes("project-upserted"), kinds.join());
        NodeAssert.ok(kinds.includes("thread-upserted"), kinds.join());

        // subscribeThread: snapshot then synchronized.
        const threadStream = rpc.stream("orchestration.subscribeThread", {
          threadId,
          requestCompletionMarker: true,
        });
        const threadValues = await threadStream.waitForValues(2);
        NodeAssert.deepEqual(
          threadValues.slice(0, 2).map((v) => (v as { kind: string }).kind),
          ["snapshot", "synchronized"],
        );

        // Resume from a sequence: replay instead of a snapshot.
        const resumed = rpc.stream("orchestration.subscribeShell", {
          afterSequence: create.body.sequence - 1,
          requestCompletionMarker: true,
        });
        const resumedValues = await resumed.waitForValues(1);
        NodeAssert.notEqual((resumedValues[0] as { kind: string }).kind, "snapshot");

        // Creating the same project root twice is refused. (The TS server reports it as a 500
        // orchestration_dispatch_failed, not a 400: the Rust server must match.)
        const duplicate = await http<{ _tag: string; reason: string }>(
          `${ctx.httpUrl}/api/orchestration/dispatch`,
          {
            method: "POST",
            headers: { cookie },
            json: {
              type: "project.create",
              commandId: NodeCrypto.randomUUID(),
              projectId: NodeCrypto.randomUUID(),
              title: "Again",
              workspaceRoot,
              createdAt: now,
            },
          },
        );
        NodeAssert.equal(duplicate.status, 500, duplicate.text);
        NodeAssert.equal(duplicate.body._tag, "EnvironmentInternalError");
        NodeAssert.equal(duplicate.body.reason, "orchestration_dispatch_failed");
        await rpc.close();
      } finally {
        NodeFS.rmSync(workspaceRoot, { recursive: true, force: true });
      }
    },
  );
});
