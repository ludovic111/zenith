#!/usr/bin/env node
/**
 * WP-31 golden gate: the telemetry and diagnostics RPCs of both backends, side by side.
 *
 *   RESOURCE_MONITOR=/path/to/t3-resource-monitor node scripts/compat/telemetry-golden.ts [--out dir]
 *
 * Each backend runs on a fresh temp home behind the validating proxy (every Chunk and Exit is
 * checked against the contracts), opens one terminal so its process tree has a child, lets
 * the monitor sample, then calls subscribeResourceTelemetry, server.getResourceTelemetryHistory,
 * server.getProcessResourceHistory, server.getProcessDiagnostics, server.getHostResources,
 * server.getTraceDiagnostics, server.retryResourceTelemetry, server.signalProcess (the server, a
 * stale identity, the terminal's shell) and POST /api/observability/v1/traces. The TS server
 * needs the sidecar binary (RESOURCE_MONITOR, built from code/native/resource-monitor).
 *
 * It prints, per RPC, the JSON paths (`a.b[].c:type`) only one backend produced; numbers and
 * strings differ by nature. With --out, the raw answers are written there.
 */
import * as NodeFS from "node:fs";
import * as NodePath from "node:path";
import * as NodeUtil from "node:util";
import { bootstrapBrowserSession, RawRpcClient } from "./lib/client.ts";
import { SYNTHETIC_SETTINGS } from "./lib/fixtures.ts";
import { formatIssue, startProxy } from "./lib/proxy.ts";
import { startServer, type Backend } from "./lib/serverUnderTest.ts";

const { values } = NodeUtil.parseArgs({ options: { out: { type: "string" } } });

const BROWSER_SPANS = {
  resourceSpans: [
    {
      resource: { attributes: [{ key: "service.name", value: { stringValue: "golden-web" } }] },
      scopeSpans: [
        {
          scope: { name: "golden" },
          spans: [
            {
              traceId: "0123456789abcdef0123456789abcdef",
              spanId: "0123456789abcdef",
              name: "golden.browser.span",
              kind: 1,
              startTimeUnixNano: "1700000000000000000",
              endTimeUnixNano: "1700000000250000000",
              attributes: [],
              droppedAttributesCount: 0,
              events: [],
              droppedEventsCount: 0,
              status: { code: 1 },
              links: [],
              droppedLinksCount: 0,
            },
          ],
        },
      ],
    },
  ],
};

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

async function run(backend: Backend): Promise<Record<string, unknown>> {
  const env: Record<string, string> = {};
  if (backend === "ts") {
    if (!process.env.RESOURCE_MONITOR) throw new Error("RESOURCE_MONITOR is not set");
    env.T3CODE_RESOURCE_MONITOR_PATH = process.env.RESOURCE_MONITOR;
  }
  const server = await startServer({
    backend,
    isolateHome: true,
    settings: SYNTHETIC_SETTINGS,
    env,
    echo: process.env.COMPAT_ECHO === "1",
  });
  const proxy = await startProxy({ target: server.httpUrl });
  const results: Record<string, unknown> = {};
  try {
    const { cookie } = await bootstrapBrowserSession(proxy.url, server.bootstrapCredential);
    if (!cookie) throw new Error("no session cookie");
    const client = await RawRpcClient.connect(proxy.wsUrl, { cookie });
    results.terminalOpen = await client.call("terminal.open", {
      threadId: "thread-golden",
      terminalId: "term-golden",
      cwd: server.homeDir,
    });
    // Background samples every 5 s.
    await sleep(6_000);
    const live = client.stream("subscribeResourceTelemetry", {});
    results.subscribeResourceTelemetry = (await live.waitForValues(2, 15_000)).slice(0, 2);
    client.interrupt(live.id);
    const window = { windowMs: 15 * 60_000, bucketMs: 60_000 };
    results.getResourceTelemetryHistory = await client.callOk("server.getResourceTelemetryHistory", window);
    results.getProcessResourceHistory = await client.callOk("server.getProcessResourceHistory", window);
    const processes = await client.callOk<{ processes: Array<{ pid: number; startTimeMs: number; command: string }> }>(
      "server.getProcessDiagnostics",
    );
    results.getProcessDiagnostics = processes;
    results.getHostResources = await client.callOk("server.getHostResources");
    results.retryResourceTelemetry = await client.callOk("server.retryResourceTelemetry");
    results.signalServer = await client.callOk("server.signalProcess", {
      pid: server.pid,
      startTimeMs: 0,
      signal: "SIGINT",
    });
    results.signalStale = await client.callOk("server.signalProcess", {
      pid: 999_999,
      startTimeMs: 0,
      signal: "SIGINT",
    });
    const shell = processes.processes[0];
    if (shell) {
      results.signalShell = await client.callOk("server.signalProcess", {
        pid: shell.pid,
        startTimeMs: shell.startTimeMs,
        signal: "SIGINT",
      });
    }
    const posted = await fetch(`${proxy.url}/api/observability/v1/traces`, {
      method: "POST",
      headers: { cookie, "content-type": "application/json" },
      body: JSON.stringify(BROWSER_SPANS),
    });
    results.postTraces = { status: posted.status, body: await posted.text() };
    const unauthenticated = await fetch(`${proxy.url}/api/observability/v1/traces`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(BROWSER_SPANS),
    });
    results.postTracesUnauthenticated = { status: unauthenticated.status };
    // The trace writer flushes every second.
    await sleep(1_500);
    results.getTraceDiagnostics = await client.callOk("server.getTraceDiagnostics");
    const config = await client.callOk<{ observability: unknown }>("server.getConfig");
    results.observability = config.observability;
    await client.close();
    results.proxyIssues = proxy.issues.map(formatIssue);
  } finally {
    await proxy.close();
    await server.stop();
  }
  return results;
}

/** Every JSON path with its type (array indices collapsed, record keys of logLevelCounts too). */
function paths(value: unknown, prefix = "", out = new Set<string>()): Set<string> {
  if (value === null) out.add(`${prefix}:null`);
  else if (Array.isArray(value)) {
    if (value.length === 0) out.add(`${prefix}:[]`);
    for (const item of value) paths(item, `${prefix}[]`, out);
  } else if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>);
    if (entries.length === 0) out.add(`${prefix}:{}`);
    for (const [key, item] of entries) {
      const name = prefix.endsWith("logLevelCounts") ? "<level>" : key;
      paths(item, prefix ? `${prefix}.${name}` : name, out);
    }
  } else out.add(`${prefix}:${typeof value}`);
  return out;
}

const ts = await run("ts");
const rust = await run("rust");
if (values.out) {
  NodeFS.mkdirSync(values.out, { recursive: true });
  NodeFS.writeFileSync(NodePath.join(values.out, "ts.json"), `${JSON.stringify(ts, null, 2)}\n`);
  NodeFS.writeFileSync(NodePath.join(values.out, "rust.json"), `${JSON.stringify(rust, null, 2)}\n`);
}
let differences = 0;
for (const key of Object.keys(ts)) {
  const left = paths(ts[key]);
  const right = paths(rust[key]);
  const onlyTs = [...left].filter((p) => !right.has(p)).sort();
  const onlyRust = [...right].filter((p) => !left.has(p)).sort();
  if (onlyTs.length === 0 && onlyRust.length === 0) {
    console.log(`= ${key}`);
    continue;
  }
  differences += 1;
  console.log(`≠ ${key}`);
  for (const p of onlyTs) console.log(`    ts only:   ${p}`);
  for (const p of onlyRust) console.log(`    rust only: ${p}`);
}
console.log(`\nproxy issues: ts ${(ts.proxyIssues as unknown[]).length}, rust ${(rust.proxyIssues as unknown[]).length}`);
for (const issue of rust.proxyIssues as string[]) console.log(`  rust: ${issue}`);
for (const issue of ts.proxyIssues as string[]) console.log(`  ts: ${issue}`);
console.log(`${differences} RPC(s) with shape differences`);
