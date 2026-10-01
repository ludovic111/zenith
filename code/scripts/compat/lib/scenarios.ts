/**
 * Scripted client sessions used to capture recordings. They speak the wire protocol like
 * apps/web does at startup and when a thread is opened, through whatever URL they are given
 * (normally the recording proxy).
 *
 * Client-generated ids and timestamps are fixed, so two captures of the same scenario differ
 * only by what the server generates.
 */
import {
  RawRpcClient,
  bootstrapBrowserSession,
  exchangeAccessToken,
  http,
  type StreamHandle,
} from "./client.ts";

export interface ScenarioContext {
  readonly httpUrl: string;
  readonly wsUrl: string;
  /** The backend's bootstrap credential (banner `Token:`). */
  readonly credential: string;
  /** Synthetic git repo (synthetic scenarios). */
  readonly workspaceRoot?: string;
  readonly log: (line: string) => void;
}

export type Scenario = (ctx: ScenarioContext) => Promise<void>;

/** Deterministic v4-shaped uuids: 00000000-0000-4000-8000-<n>. */
const fixedUuid = (n: number) => `00000000-0000-4000-8000-${n.toString(16).padStart(12, "0")}`;
const FIXED_TIME = "2026-01-01T00:00:00.000Z";

/** Query parameters the web client puts on /ws (plan §1.3 rule 11). */
const WEB_QUERY =
  "clientSurface=web&clientAppVersion=0.0.43&clientDeviceType=desktop&clientOs=macOS" +
  "&clientBrowser=Chrome&connectionMethod=direct&orchestrationProtocol=1";

const settle = (ms: number) => new Promise((r) => setTimeout(r, ms));

const waitKind = async (stream: StreamHandle, kind: string, timeoutMs = 10_000) => {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const found = stream.values.find((v) => (v as { kind?: string }).kind === kind);
    if (found) return found;
    const remaining = deadline - Date.now();
    if (remaining <= 0 || stream.exit) {
      throw new Error(`${stream.tag}: no "${kind}" item within ${timeoutMs} ms`);
    }
    await stream.waitForValues(stream.values.length + 1, remaining).catch(() => {});
  }
};

/** What the web app does right after connecting (minus its periodic pings and activity reports). */
const openWebSession = async (ctx: ScenarioContext) => {
  await http(`${ctx.httpUrl}/.well-known/t3/environment`);
  await http(`${ctx.httpUrl}/api/auth/session`);
  const boot = await bootstrapBrowserSession(ctx.httpUrl, ctx.credential);
  if (boot.status !== 200) throw new Error(`browser-session failed: ${boot.status} ${boot.text}`);
  const cookie = boot.cookie!;
  await http(`${ctx.httpUrl}/api/auth/session`, { headers: { cookie } });
  await http(`${ctx.httpUrl}/zenith/embed.json`);
  const rpc = await RawRpcClient.connect(`${ctx.wsUrl}?${WEB_QUERY}`, { cookie });
  const config = rpc.stream("subscribeServerConfig", {
    environmentThemes: true,
    usageLimitSources: true,
    usageLimitsCommand: true,
  });
  const lifecycle = rpc.stream("subscribeServerLifecycle", {});
  const access = rpc.stream("subscribeAuthAccess", {});
  const shell = rpc.stream("orchestration.subscribeShell", { requestCompletionMarker: true });
  const background = rpc.stream("subscribeBackgroundPolicy", {});
  await config.waitForValues(1);
  await lifecycle.waitForValues(1);
  await access.waitForValues(1);
  await waitKind(shell, "synchronized");
  await background.waitForValues(1);
  for (const tag of [
    "server.probe",
    "server.getConfig",
    "server.getSettings",
    "server.getBackgroundPolicy",
    "cloud.getRelayClientStatus",
    "orchestration.getArchivedShellSnapshot",
  ]) {
    await rpc.call(tag, {}, 30_000);
  }
  return { rpc, cookie, config, lifecycle, access, shell };
};

/**
 * Web startup, then a synthetic project and thread: created, listed, opened (HTTP snapshot +
 * subscribeThread resume), renamed; workspace and VCS reads on the synthetic repo; a keybinding
 * upserted and removed; a few error shapes. No provider turn is ever started.
 */
export const webSession: Scenario = async (ctx) => {
  if (!ctx.workspaceRoot) throw new Error("webSession needs a workspaceRoot");
  const cwd = ctx.workspaceRoot;
  const { rpc, cookie, shell } = await openWebSession(ctx);
  const projectId = fixedUuid(1);
  const threadId = fixedUuid(2);

  ctx.log("dispatch project.create / thread.create");
  await rpc.callOk("orchestration.dispatchCommand", {
    type: "project.create",
    commandId: fixedUuid(101),
    projectId,
    title: "Synthetic project",
    workspaceRoot: cwd,
    createdAt: FIXED_TIME,
  });
  await waitKind(shell, "project-upserted");
  await rpc.callOk("orchestration.dispatchCommand", {
    type: "thread.create",
    commandId: fixedUuid(102),
    threadId,
    projectId,
    title: "Synthetic thread",
    modelSelection: { instanceId: "codex", model: "gpt-5-codex" },
    runtimeMode: "approval-required",
    interactionMode: "default",
    branch: "main",
    worktreePath: null,
    createdAt: FIXED_TIME,
  });
  await waitKind(shell, "thread-upserted");

  ctx.log("open the thread");
  await http(`${ctx.httpUrl}/api/orchestration/shell`, { headers: { cookie } });
  const detail = await http<{ snapshotSequence: number }>(
    `${ctx.httpUrl}/api/orchestration/threads/${encodeURIComponent(threadId)}?reasoningMessages=true&turnLimit=20`,
    { headers: { cookie } },
  );
  const thread = rpc.stream("orchestration.subscribeThread", {
    threadId,
    reasoningMessages: true,
    afterSequence: detail.body.snapshotSequence,
    requestCompletionMarker: true,
  });
  await waitKind(thread, "synchronized");
  const worktree = rpc.stream("subscribeWorktreeSetup", { threadId });
  await worktree.waitForValues(1);
  await rpc.call("preview.list", { threadId });

  ctx.log("workspace and vcs reads");
  await rpc.call("projects.listEntries", { cwd });
  await rpc.call("projects.listEntries", { cwd, directoryPath: "" });
  await rpc.call("projects.searchEntries", { cwd, query: "needle", limit: 10 });
  await rpc.call("projects.searchEntries", { cwd, query: "", limit: 10 });
  await rpc.call("projects.readFile", { cwd, relativePath: "README.md" });
  await rpc.call("projects.readFile", { cwd, relativePath: "does-not-exist.md" });
  await rpc.call("vcs.refreshStatus", { cwd }, 30_000);
  const vcs = rpc.stream("subscribeVcsStatus", { cwd });
  await vcs.waitForValues(1, 30_000);
  await rpc.call("vcs.listRefs", { cwd }, 30_000);

  ctx.log("rename the thread");
  await rpc.callOk("orchestration.dispatchCommand", {
    type: "thread.meta.update",
    commandId: fixedUuid(103),
    threadId,
    title: "Synthetic thread (renamed)",
  });
  await thread.waitForValues(thread.values.length + 1).catch(() => {});
  await rpc.call("orchestration.searchThreads", { query: "Synthetic" });

  ctx.log("keybindings");
  await rpc.call("server.upsertKeybinding", { key: "mod+shift+y", command: "sidebar.toggle" });
  await rpc.call("server.removeKeybinding", { key: "mod+shift+y", command: "sidebar.toggle" });

  ctx.log("error shapes");
  await rpc.call("foo.bar", {});
  await rpc.call("orchestration.subscribeThread", { threadId: 42 });
  await http(`${ctx.httpUrl}/api/orchestration/threads/${fixedUuid(999)}`, { headers: { cookie } });
  await rpc
    .callOk("orchestration.dispatchCommand", {
      type: "project.create",
      commandId: fixedUuid(104),
      projectId: fixedUuid(3),
      title: "Duplicate",
      workspaceRoot: cwd,
      createdAt: FIXED_TIME,
    })
    .catch(() => {});

  rpc.interrupt(thread.id);
  await settle(500);
  await rpc.close();
};

/** Pairing, tokens, tickets, access management and their error shapes (HTTP + one socket). */
export const authFlows: Scenario = async (ctx) => {
  await http(`${ctx.httpUrl}/api/auth/session`);
  const boot = await bootstrapBrowserSession(ctx.httpUrl, ctx.credential);
  const owner = boot.cookie!;
  await bootstrapBrowserSession(ctx.httpUrl, ctx.credential); // one-time: 401
  await http(`${ctx.httpUrl}/api/auth/pairing-token`, { method: "POST", json: {} }); // 401
  const phone = await http<{ id: string; credential: string }>(
    `${ctx.httpUrl}/api/auth/pairing-token`,
    {
      method: "POST",
      headers: { cookie: owner },
      json: { label: "Synthetic phone" },
    },
  );
  await http(`${ctx.httpUrl}/api/auth/pairing-token`, {
    method: "POST",
    headers: { cookie: owner },
    json: { scopes: [] },
  }); // 400 invalid_scope
  const reader = await http<{ credential: string }>(`${ctx.httpUrl}/api/auth/pairing-token`, {
    method: "POST",
    headers: { cookie: owner },
    json: { label: "Reader", scopes: ["access:read"] },
  });
  await http(`${ctx.httpUrl}/api/auth/pairing-links`, { headers: { cookie: owner } });
  const token = await exchangeAccessToken(ctx.httpUrl, reader.body.credential, {
    scope: "access:read",
  });
  const bearer = `Bearer ${token.body.access_token}`;
  await http(`${ctx.httpUrl}/api/auth/session`, { headers: { authorization: bearer } });
  await http(`${ctx.httpUrl}/api/auth/pairing-token`, {
    method: "POST",
    headers: { authorization: bearer },
    json: {},
  }); // 403
  const ticket = await http<{ ticket: string }>(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
    method: "POST",
    headers: { authorization: bearer },
  });
  const rpc = await RawRpcClient.connect(
    `${ctx.wsUrl}?wsTicket=${encodeURIComponent(ticket.body.ticket)}`,
  );
  const access = rpc.stream("subscribeAuthAccess", {});
  await access.waitForValues(1);
  await rpc.call("server.getConfig", {}); // EnvironmentAuthorizationError
  await http(`${ctx.httpUrl}/api/auth/pairing-links/revoke`, {
    method: "POST",
    headers: { cookie: owner },
    json: { id: phone.body.id },
  });
  await access.waitForValues(2, 3000).catch(() => {});
  await bootstrapBrowserSession(ctx.httpUrl, phone.body.credential); // revoked: 401
  await http(`${ctx.httpUrl}/api/auth/clients`, { headers: { cookie: owner } });
  await http(`${ctx.httpUrl}/api/auth/clients/revoke-others`, {
    method: "POST",
    headers: { cookie: owner },
  });
  await http(`${ctx.httpUrl}/api/auth/session`, { headers: { authorization: bearer } }); // revoked
  await http(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
    method: "OPTIONS",
    headers: {
      origin: "http://remote-client.test:3773",
      "access-control-request-method": "POST",
      "access-control-request-headers": "authorization",
    },
  });
  await settle(300);
  await rpc.close();
};

/**
 * Read-only sweep for a copy of a live home: startup, both snapshots, and the most recently
 * updated threads opened like the web opens them. Never dispatches, never touches VCS.
 */
export const liveReadOnly =
  (threadCount = 8): Scenario =>
  async (ctx) => {
    const { rpc, cookie, shell } = await openWebSession(ctx);
    await http(`${ctx.httpUrl}/api/orchestration/shell`, { headers: { cookie } });
    await http(`${ctx.httpUrl}/api/orchestration/snapshot`, { headers: { cookie } });
    const snapshot = (
      shell.values.find((v) => (v as { kind?: string }).kind === "snapshot") as {
        snapshot: {
          threads: Array<{ id: string; updatedAt?: string; archivedAt?: string | null }>;
        };
      }
    )?.snapshot;
    const threads = [...(snapshot?.threads ?? [])]
      .filter((t) => !t.archivedAt)
      .sort((a, b) => String(b.updatedAt).localeCompare(String(a.updatedAt)))
      .slice(0, threadCount);
    ctx.log(`opening ${threads.length} threads`);
    for (const t of threads) {
      const detail = await http<{ snapshotSequence: number }>(
        `${ctx.httpUrl}/api/orchestration/threads/${encodeURIComponent(t.id)}?reasoningMessages=true&turnLimit=20`,
        { headers: { cookie } },
      );
      const stream = rpc.stream("orchestration.subscribeThread", {
        threadId: t.id,
        reasoningMessages: true,
        ...(detail.status === 200 ? { afterSequence: detail.body.snapshotSequence } : {}),
        requestCompletionMarker: true,
      });
      await waitKind(stream, "synchronized", 30_000).catch((e) => ctx.log(String(e)));
      const full = rpc.stream("orchestration.subscribeThread", {
        threadId: t.id,
        reasoningMessages: true,
        requestCompletionMarker: true,
      });
      await waitKind(full, "synchronized", 30_000).catch((e) => ctx.log(String(e)));
      rpc.interrupt(stream.id);
      rpc.interrupt(full.id);
      const worktree = rpc.stream("subscribeWorktreeSetup", { threadId: t.id });
      await worktree.waitForValues(1, 5000).catch(() => {});
      rpc.interrupt(worktree.id);
    }
    await settle(500);
    await rpc.close();
  };

export const SCENARIOS: Record<string, Scenario> = {
  "web-session": webSession,
  auth: authFlows,
  "live-read-only": liveReadOnly(),
};
