/**
 * The `/ws` endpoint, black-box: handshake auth, permessage-deflate, the Effect-RPC envelope rules
 * of plan §1.3, and a few RPCs through the real typed client.
 * Ported from apps/server/src/server.test.ts where a line number is given.
 */
import * as NodeTest from "node:test";
import * as NodeAssert from "node:assert/strict";
import { Effect } from "effect";
import { RawRpcClient, WsUpgradeRejected, exchangeAccessToken, http } from "../lib/client.ts";
import { withWsRpcClient } from "../lib/effectRpc.ts";
import { WebSocket } from "../lib/ws.ts";
import { useServer } from "./harness.ts";

const openSocket = (url: string, options: { cookie?: string; perMessageDeflate: boolean }) =>
  new Promise<InstanceType<typeof WebSocket>>((resolve, reject) => {
    const socket = new WebSocket(url, {
      perMessageDeflate: options.perMessageDeflate,
      ...(options.cookie ? { headers: { cookie: options.cookie } } : {}),
    });
    socket.once("open", () => resolve(socket));
    socket.once("unexpected-response", (_req: unknown, res: { statusCode?: number }) =>
      reject(new WsUpgradeRejected(res.statusCode ?? 0)),
    );
    socket.once("error", reject);
  });

NodeTest.describe("ws: handshake", () => {
  const ctx = useServer();

  // server.test.ts:4503
  NodeTest.test("negotiates permessage-deflate with clients that offer it", async () => {
    const cookie = await ctx.adminCookie();
    const compressed = await openSocket(ctx.server.wsUrl, { cookie, perMessageDeflate: true });
    NodeAssert.ok(compressed.extensions.includes("permessage-deflate"), compressed.extensions);
    compressed.close();
    const plain = await openSocket(ctx.server.wsUrl, { cookie, perMessageDeflate: false });
    NodeAssert.ok(!plain.extensions.includes("permessage-deflate"), plain.extensions);
    plain.close();
  });

  // server.test.ts:6374
  NodeTest.test("rejects the upgrade when session authentication is missing", async () => {
    await NodeAssert.rejects(RawRpcClient.connect(ctx.wsUrl), (error: unknown) => {
      NodeAssert.ok(error instanceof WsUpgradeRejected, String(error));
      NodeAssert.equal(error.status, 401);
      return true;
    });
  });

  // server.test.ts:5397
  NodeTest.test(
    "rejects the upgrade when a session token is only provided via query string",
    async () => {
      const cookie = await ctx.newSessionCookie();
      const token = cookie.split("=").slice(1).join("=");
      await NodeAssert.rejects(
        RawRpcClient.connect(`${ctx.wsUrl}?token=${encodeURIComponent(token)}`),
        WsUpgradeRejected,
      );
    },
  );

  // server.test.ts:5311 (adapted: loopback-browser policy)
  NodeTest.test(
    "accepts the RPC handshake with a browser session cookie (real Effect RPC client)",
    async () => {
      const cookie = await ctx.adminCookie();
      const descriptor = await http<{ environmentId: string }>(
        `${ctx.httpUrl}/.well-known/t3/environment`,
      );
      const result = await withWsRpcClient(ctx.wsUrl, (client) => client["server.getConfig"]({}), {
        cookie,
      });
      NodeAssert.ok(result.ok, result.ok ? "" : result.message);
      NodeAssert.equal(result.value.environment.environmentId, descriptor.body.environmentId);
      NodeAssert.equal(result.value.auth.policy, "loopback-browser");
      NodeAssert.equal(result.value.shellResumeCompletionMarker, true);
      NodeAssert.equal(result.value.threadResumeCompletionMarker, true);
    },
  );

  // server.test.ts:5417
  NodeTest.test(
    "accepts the RPC handshake with a dedicated websocket ticket in the query string",
    async () => {
      const bearer = await ctx.newBearerToken();
      const ticket = await http<{ ticket: string }>(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
        method: "POST",
        headers: { authorization: `Bearer ${bearer}` },
      });
      const url = `${ctx.wsUrl}?wsTicket=${encodeURIComponent(ticket.body.ticket)}`;
      const result = await withWsRpcClient(url, (client) => client["server.getConfig"]({}));
      NodeAssert.ok(result.ok, result.ok ? "" : result.message);
      NodeAssert.equal(result.value.auth.policy, "loopback-browser");
    },
  );

  NodeTest.test("accepts a bearer access token in the Authorization header", async () => {
    const bearer = await ctx.newBearerToken();
    const rpc = await RawRpcClient.connect(ctx.wsUrl, {
      headers: { authorization: `Bearer ${bearer}` },
    });
    const exit = await rpc.call("server.getSettings", {});
    NodeAssert.equal(exit._tag, "Success");
    await rpc.close();
  });

  // server.test.ts:4555 (RPC half)
  NodeTest.test("enforces per-RPC scopes with EnvironmentAuthorizationError", async () => {
    const token = await exchangeAccessToken(ctx.httpUrl, await ctx.newCredential(), {
      scope: "access:write",
    });
    const ticket = await http<{ ticket: string }>(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
      method: "POST",
      headers: { authorization: `Bearer ${token.body.access_token}` },
    });
    const url = `${ctx.wsUrl}?wsTicket=${encodeURIComponent(ticket.body.ticket)}`;
    const result = await withWsRpcClient(url, (client) => client["server.getConfig"]({}));
    NodeAssert.equal(result.ok, false);
    const failure = (result as { failure: { _tag: string; requiredScope?: string } }).failure;
    NodeAssert.equal(failure._tag, "EnvironmentAuthorizationError");
    NodeAssert.equal(failure.requiredScope, "orchestration:read");
  });
});

NodeTest.describe("ws: Effect RPC envelope (plan §1.3)", () => {
  const ctx = useServer();

  NodeTest.test("answers Ping with Pong", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    rpc.ping();
    const deadline = Date.now() + 5000;
    while (
      !rpc.frames.some((f) => (f as { _tag?: string })._tag === "Pong") &&
      Date.now() < deadline
    ) {
      await new Promise((r) => setTimeout(r, 20));
    }
    NodeAssert.ok(
      rpc.frames.some((f) => (f as { _tag?: string })._tag === "Pong"),
      JSON.stringify(rpc.frames),
    );
    await rpc.close();
  });

  NodeTest.test("an unknown tag gets a per-request Exit(Die), not a Defect", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    const exit = await rpc.call("foo.bar", {});
    NodeAssert.deepEqual(exit, {
      _tag: "Failure",
      cause: [{ _tag: "Die", defect: "Unknown request tag: foo.bar" }],
    });
    // The socket is still usable.
    NodeAssert.equal((await rpc.call("server.getSettings", {}))._tag, "Success");
    NodeAssert.ok(!rpc.frames.some((f) => (f as { _tag?: string })._tag === "Defect"));
    await rpc.close();
  });

  NodeTest.test("a payload that fails to decode gets a per-request Exit(Die)", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    const exit = await rpc.call("orchestration.subscribeThread", { threadId: 42 });
    NodeAssert.equal(exit._tag, "Failure");
    const causes = (exit as { cause: Array<{ _tag: string }> }).cause;
    NodeAssert.equal(causes[0]?._tag, "Die");
    NodeAssert.ok(!rpc.frames.some((f) => (f as { _tag?: string })._tag === "Defect"));
    await rpc.close();
  });

  NodeTest.test("echoes string request ids with the same JSON type", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    rpc.send({ _tag: "Request", id: "abc", tag: "server.getSettings", payload: {}, headers: [] });
    const deadline = Date.now() + 5000;
    let exit: { requestId?: unknown } | undefined;
    while (!exit && Date.now() < deadline) {
      exit = rpc.frames.find((f) => (f as { _tag?: string })._tag === "Exit") as typeof exit;
      await new Promise((r) => setTimeout(r, 20));
    }
    NodeAssert.ok(exit);
    NodeAssert.equal(exit.requestId, "abc");
    await rpc.close();
  });

  NodeTest.test(
    "a stream sends one item per Ack and never completes subscribeServerConfig",
    async () => {
      const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
      const config = rpc.stream("subscribeServerConfig", {});
      const [first] = await config.waitForValues(1);
      NodeAssert.equal((first as { type: string }).type, "snapshot");
      NodeAssert.equal((first as { version: number }).version, 1);
      await new Promise((r) => setTimeout(r, 300));
      NodeAssert.equal(config.exit, undefined);
      await rpc.close();
    },
  );

  NodeTest.test("subscribeShell on an empty home: snapshot then synchronized", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    const shell = rpc.stream("orchestration.subscribeShell", { requestCompletionMarker: true });
    const values = await shell.waitForValues(2);
    NodeAssert.deepEqual(
      values.map((v) => (v as { kind: string }).kind),
      ["snapshot", "synchronized"],
    );
    const snapshot = (values[0] as { snapshot: Record<string, unknown> }).snapshot;
    NodeAssert.deepEqual(snapshot.projects, []);
    NodeAssert.deepEqual(snapshot.threads, []);
    NodeAssert.equal(snapshot.snapshotSequence, 0);
    await rpc.close();
  });

  NodeTest.test("Interrupt cancels a stream", async () => {
    const rpc = await RawRpcClient.connect(ctx.wsUrl, { cookie: await ctx.adminCookie() });
    const lifecycle = rpc.stream("subscribeServerLifecycle", {});
    await lifecycle.waitForValues(1);
    rpc.interrupt(lifecycle.id);
    // Still alive afterwards.
    NodeAssert.equal((await rpc.call("server.getSettings", {}))._tag, "Success");
    await rpc.close();
  });

  // server.test.ts:4915
  NodeTest.test(
    "access-read sockets get pairing metadata only, in snapshots and updates",
    async () => {
      const owner = await ctx.adminCookie();
      const createLink = async () => {
        const res = await http<{ id: string; credential: string }>(
          `${ctx.httpUrl}/api/auth/pairing-token`,
          {
            method: "POST",
            headers: { cookie: owner },
            json: {},
          },
        );
        NodeAssert.equal(res.status, 200);
        return res.body;
      };
      const initialLink = await createLink();
      const reader = await exchangeAccessToken(ctx.httpUrl, await ctx.newCredential(), {
        scope: "access:read",
      });
      NodeAssert.equal(reader.body.scope, "access:read");
      const ticket = await http<{ ticket: string }>(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
        method: "POST",
        headers: { authorization: `Bearer ${reader.body.access_token}` },
      });
      const rpc = await RawRpcClient.connect(
        `${ctx.wsUrl}?wsTicket=${encodeURIComponent(ticket.body.ticket)}`,
      );
      const access = rpc.stream("subscribeAuthAccess", {});
      const [snapshot] = (await access.waitForValues(1)) as Array<{
        type: string;
        revision: number;
        payload: { pairingLinks: Array<{ id: string }> };
      }>;
      NodeAssert.equal(snapshot!.type, "snapshot");
      NodeAssert.ok(snapshot!.payload.pairingLinks.some((link) => link.id === initialLink.id));
      // The original test waits on an internal "changes subscribed" hook; black-box, we retry.
      let liveLink = await createLink();
      type AccessEvent = { type: string; payload: { id: string } };
      let update: AccessEvent | undefined;
      for (let attempt = 0; attempt < 5 && !update; attempt++) {
        try {
          await access.waitForValues(2, 1000);
        } catch {
          liveLink = await createLink();
          continue;
        }
        update = (access.values as Array<AccessEvent>).find(
          (v) => v.type === "pairingLinkUpserted",
        );
      }
      NodeAssert.ok(update, JSON.stringify(access.values));
      const wire = JSON.stringify(rpc.frames);
      NodeAssert.ok(!wire.includes('"credential"'));
      NodeAssert.ok(!wire.includes(initialLink.credential));
      NodeAssert.ok(!wire.includes(liveLink.credential));
      const paired = await exchangeAccessToken(ctx.httpUrl, liveLink.credential, {
        scope: "orchestration:read",
      });
      NodeAssert.equal(paired.status, 200);
      await rpc.close();
    },
  );

  NodeTest.test(
    "the typed client decodes a server.getSettings / server.getConfig round trip",
    async () => {
      const cookie = await ctx.adminCookie();
      const result = await withWsRpcClient(
        ctx.wsUrl,
        (client) => Effect.all([client["server.getSettings"]({}), client["server.getConfig"]({})]),
        { cookie },
      );
      NodeAssert.ok(result.ok, result.ok ? "" : result.message);
    },
  );
});
