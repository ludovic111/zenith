#!/usr/bin/env node
/**
 * Captures a recording: starts a backend on a temp home, puts the recording proxy in front of it,
 * runs a scripted client session through the proxy, and writes the JSONL.
 *
 *   node scripts/compat/capture.ts --scenario web-session --out scripts/compat/recordings/web-session.jsonl
 *   node scripts/compat/capture.ts --scenario auth        --out scripts/compat/recordings/auth.jsonl
 *   node scripts/compat/capture.ts --scenario live-read-only --home live --out /tmp/live.jsonl
 *
 * --home synthetic (default)  fresh empty home + a deterministic git repo; nothing personal, so
 *                             the recording is not redacted (its tokens died with the temp server)
 *                             and can be committed and replayed.
 * --home live                 a *safe copy* of --live-source (default ~/.zenith/code; see
 *                             lib/fixtures.ts copyLiveHome). The source is only read. Recorded
 *                             with redaction, and refused inside recordings/: it holds real
 *                             thread content, keep it local.
 * --backend ts|rust           default: $BACKEND or ts.
 *
 * --scenario interactive       records a real apps/web session: seeds a synthetic project and
 *                              thread through the proxy, mints a pairing credential, prints the
 *                              pairing URL (open it in a browser), and records until Ctrl-C.
 *                              --static <dir> serves a built web client (e.g. the main checkout's
 *                              apps/server/dist/client) for backends that do not serve one.
 */
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import * as NodeUtil from "node:util";
import { copyLiveHome, makeSyntheticRepo, SYNTHETIC_SETTINGS } from "./lib/fixtures.ts";
import { bootstrapBrowserSession, http } from "./lib/client.ts";
import { formatIssue, startProxy, summarizeIssues } from "./lib/proxy.ts";
import { machineScrubs } from "./lib/recording.ts";
import { SCENARIOS, type ScenarioContext } from "./lib/scenarios.ts";
import { backendFromEnv, startServer, type Backend } from "./lib/serverUnderTest.ts";

const { values } = NodeUtil.parseArgs({
  options: {
    scenario: { type: "string" },
    out: { type: "string" },
    backend: { type: "string" },
    home: { type: "string", default: "synthetic" },
    "live-source": { type: "string", default: NodePath.join(NodeOS.homedir(), ".zenith/code") },
    strict: { type: "boolean", default: false },
    keep: { type: "boolean", default: false },
    echo: { type: "boolean", default: false },
    static: { type: "string" },
    port: { type: "string", default: "0" },
  },
});

const scenario =
  values.scenario === "interactive"
    ? interactive
    : values.scenario
      ? SCENARIOS[values.scenario]
      : undefined;
if (!scenario || !values.out) {
  process.stderr.write(
    `usage: node scripts/compat/capture.ts --scenario <${[...Object.keys(SCENARIOS), "interactive"].join("|")}> --out <file.jsonl> [--home synthetic|live] [--backend ts|rust] [--strict] [--keep] [--static <dir>] [--port <n>]\n`,
  );
  process.exit(2);
}
const live = values.home === "live";
const out = NodePath.resolve(values.out);
const recordingsDir = NodePath.resolve(import.meta.dirname, "recordings");
if (live && out.startsWith(recordingsDir)) {
  process.stderr.write(
    "refusing to write a live-home recording into recordings/: it contains real data\n",
  );
  process.exit(2);
}
const backend = (values.backend as Backend | undefined) ?? backendFromEnv();
const log = (line: string) => process.stderr.write(`capture: ${line}\n`);

const tmp = NodeFS.realpathSync(
  NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-capture-")),
);
const home = NodePath.join(tmp, "home");
NodeFS.mkdirSync(home, { recursive: true });
let workspaceRoot: string | undefined;
if (live) {
  log(`copying ${values["live-source"]} (read-only) into ${home}`);
  copyLiveHome(values["live-source"]!, home);
} else {
  workspaceRoot = makeSyntheticRepo(NodePath.join(tmp, "workspace"));
}

// Synthetic captures run with an empty HOME: no provider login, nothing of this machine.
const server = await startServer({
  backend,
  homeDir: home,
  isolateHome: !live,
  ...(live ? {} : { settings: SYNTHETIC_SETTINGS }),
  echo: values.echo,
});
log(`${backend} backend on ${server.httpUrl}`);
const descriptor = await http<{ label?: string }>(`${server.httpUrl}/.well-known/t3/environment`);
const proxy = await startProxy({
  target: server.httpUrl,
  port: Number(values.port),
  ...(values.static ? { staticDir: values.static } : {}),
  recordFile: out,
  redact: live,
  scrub: machineScrubs(descriptor.body.label ? [descriptor.body.label] : []),
  strict: values.strict,
  onIssue: (issue) => log(formatIssue(issue)),
});
const aliases: Record<string, string> = { [home]: "<home>", [tmp]: "<tmp>" };
if (workspaceRoot) aliases[workspaceRoot] = "<workspace>";
if (server.userHomeDir) aliases[server.userHomeDir] = "<user-home>";
for (const [from, to] of Object.entries({ ...aliases })) {
  if (from.startsWith("/private/")) aliases[from.slice("/private".length)] = to;
}
proxy.recording!.write("meta", "meta", {
  kind: "capture",
  scenario: values.scenario,
  backend,
  home: live ? "live-copy" : "synthetic",
  ...(live ? {} : { bootstrapCredential: server.bootstrapCredential }),
  ...(workspaceRoot ? { workspaceRoot } : {}),
  pathAliases: aliases,
});

let failed: unknown;
try {
  await scenario({
    httpUrl: proxy.url,
    wsUrl: proxy.wsUrl,
    credential: server.bootstrapCredential,
    ...(workspaceRoot ? { workspaceRoot } : {}),
    log,
  });
} catch (error) {
  failed = error;
  log(`scenario failed: ${error instanceof Error ? error.stack : String(error)}`);
}
await proxy.close();
await server.stop();
if (!values.keep) NodeFS.rmSync(tmp, { recursive: true, force: true });
else log(`kept ${tmp}`);

const summary = summarizeIssues(proxy.issues);
process.stdout.write(
  `${JSON.stringify(
    {
      out,
      http: proxy.stats.http,
      ws: proxy.stats.ws,
      wsClientFrames: proxy.stats.wsClientFrames,
      wsServerFrames: proxy.stats.wsServerFrames,
      rpcTags: Object.fromEntries(proxy.stats.rpcTags),
      endpoints: Object.fromEntries(proxy.stats.endpoints),
      ...summary,
    },
    null,
    2,
  )}\n`,
);
process.exit(failed || summary.errors > 0 ? 1 : 0);

/** Seeds through the proxy (so a replay re-creates it), then lets a human drive apps/web. */
async function interactive(ctx: ScenarioContext): Promise<void> {
  const boot = await bootstrapBrowserSession(ctx.httpUrl, ctx.credential);
  const cookie = boot.cookie!;
  if (ctx.workspaceRoot) {
    const dispatch = (json: unknown) =>
      http(`${ctx.httpUrl}/api/orchestration/dispatch`, {
        method: "POST",
        headers: { cookie },
        json,
      });
    await dispatch({
      type: "project.create",
      commandId: "00000000-0000-4000-8000-000000000201",
      projectId: "00000000-0000-4000-8000-000000000001",
      title: "Synthetic project",
      workspaceRoot: ctx.workspaceRoot,
      createdAt: "2026-01-01T00:00:00.000Z",
    });
    await dispatch({
      type: "thread.create",
      commandId: "00000000-0000-4000-8000-000000000202",
      threadId: "00000000-0000-4000-8000-000000000002",
      projectId: "00000000-0000-4000-8000-000000000001",
      title: "Synthetic thread",
      modelSelection: { instanceId: "codex", model: "gpt-5-codex" },
      runtimeMode: "approval-required",
      interactionMode: "default",
      branch: "main",
      worktreePath: null,
      createdAt: "2026-01-01T00:00:00.000Z",
    });
  }
  const pairing = await http<{ credential: string }>(`${ctx.httpUrl}/api/auth/pairing-token`, {
    method: "POST",
    headers: { cookie },
    json: { label: "Browser" },
  });
  process.stdout.write(
    `\nOpen ${ctx.httpUrl}/pair#token=${pairing.body.credential}\nPress Ctrl-C (or send SIGINT) to stop recording.\n\n`,
  );
  await new Promise<void>((resolve) => {
    process.once("SIGINT", () => resolve());
    process.once("SIGTERM", () => resolve());
  });
}
