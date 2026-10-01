#!/usr/bin/env node
/**
 * Replays the client side of a recording against a backend, through the validating proxy, and
 * writes what the backend answered as a new recording (same format, same connection names).
 *
 *   # start a fresh backend (BACKEND=ts|rust) the way the capture did, replay, then diff:
 *   BACKEND=rust node scripts/compat/replay.ts --recording scripts/compat/recordings/web-session.jsonl \
 *     --out /tmp/web-session.rust.jsonl --diff
 *
 *   # or against a backend you started yourself:
 *   node scripts/compat/replay.ts --recording rec.jsonl --target http://127.0.0.1:4800 --credential ABCD… --out out.jsonl
 *
 * When it starts the backend itself, a synthetic recording gets the same setup as at capture
 * time: an empty home, an empty HOME, and the deterministic git repo, whose path is substituted
 * for the recorded one in every client frame.
 */
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import * as NodeUtil from "node:util";
import { http } from "./lib/client.ts";
import { diffRecordings, formatDiffReport } from "./lib/diff.ts";
import { makeSyntheticRepo, SYNTHETIC_SETTINGS } from "./lib/fixtures.ts";
import { formatIssue, startProxy, summarizeIssues } from "./lib/proxy.ts";
import { machineScrubs, readRecording } from "./lib/recording.ts";
import { replayRecording } from "./lib/replay.ts";
import {
  backendFromEnv,
  startServer,
  type Backend,
  type ServerHandle,
} from "./lib/serverUnderTest.ts";

const { values } = NodeUtil.parseArgs({
  options: {
    recording: { type: "string" },
    out: { type: "string" },
    backend: { type: "string" },
    target: { type: "string" },
    credential: { type: "string" },
    diff: { type: "boolean", default: false },
    sequences: { type: "string", default: "ordinal" },
    "causal-timeout": { type: "string", default: "5000" },
    settle: { type: "string", default: "1000" },
    echo: { type: "boolean", default: false },
  },
});
if (!values.recording || !values.out) {
  process.stderr.write(
    "usage: node scripts/compat/replay.ts --recording <in.jsonl> --out <out.jsonl> [--backend ts|rust | --target <url> --credential <token>] [--diff]\n",
  );
  process.exit(2);
}
const log = (line: string) => process.stderr.write(`replay: ${line}\n`);
const events = readRecording(values.recording);
const meta = (events.find(
  (e) => e.dir === "meta" && (e.frame as { kind?: string }).kind === "capture",
)?.frame ?? {}) as { workspaceRoot?: string; home?: string; pathAliases?: Record<string, string> };

let server: ServerHandle | undefined;
let tmp: string | undefined;
const substitutions: Record<string, string> = {};
const pathAliases: Record<string, string> = {};
let target: { httpUrl: string; credential?: string };
if (values.target) {
  target = {
    httpUrl: values.target,
    ...(values.credential ? { credential: values.credential } : {}),
  };
} else {
  const backend = (values.backend as Backend | undefined) ?? backendFromEnv();
  tmp = NodeFS.realpathSync(
    NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-replay-")),
  );
  const home = NodePath.join(tmp, "home");
  NodeFS.mkdirSync(home);
  pathAliases[home] = "<home>";
  pathAliases[tmp] = "<tmp>";
  if (meta.workspaceRoot) {
    const workspace = makeSyntheticRepo(NodePath.join(tmp, "workspace"));
    substitutions[meta.workspaceRoot] = workspace;
    pathAliases[workspace] = "<workspace>";
  }
  const synthetic = meta.home !== "live-copy";
  server = await startServer({
    backend,
    homeDir: home,
    isolateHome: synthetic,
    ...(synthetic ? { settings: SYNTHETIC_SETTINGS } : {}),
    echo: values.echo,
  });
  if (server.userHomeDir) pathAliases[server.userHomeDir] = "<user-home>";
  for (const [from, to] of Object.entries({ ...pathAliases })) {
    if (from.startsWith("/private/")) pathAliases[from.slice("/private".length)] = to;
  }
  log(`${backend} backend on ${server.httpUrl}`);
  target = { httpUrl: server.httpUrl, credential: server.bootstrapCredential };
}

const descriptor = await http<{ label?: string }>(`${target.httpUrl}/.well-known/t3/environment`);
const proxy = await startProxy({
  target: target.httpUrl,
  onIssue: (issue) => log(formatIssue(issue)),
});
const result = await replayRecording({
  events,
  target: {
    httpUrl: proxy.url,
    wsUrl: proxy.wsUrl,
    ...(target.credential ? { bootstrapCredential: target.credential } : {}),
  },
  outFile: values.out,
  // Same machine scrubbing as captures, so the two compare.
  scrub: machineScrubs(descriptor.body.label ? [descriptor.body.label] : []),
  substitutions,
  pathAliases,
  causalTimeoutMs: Number(values["causal-timeout"]),
  settleMs: Number(values.settle),
  log,
});
await proxy.close();
await server?.stop();
if (tmp) NodeFS.rmSync(tmp, { recursive: true, force: true });

const replayed = result.events;

const summary = summarizeIssues(proxy.issues);
process.stdout.write(
  `${JSON.stringify({ out: NodePath.resolve(values.out), warnings: result.warnings.length, validation: summary }, null, 2)}\n`,
);
let exitCode = summary.errors > 0 ? 1 : 0;
if (values.diff) {
  const diffs = diffRecordings(events, replayed, {
    sequences: values.sequences as "ordinal" | "rebase" | "exact",
  });
  process.stdout.write(`${formatDiffReport(diffs)}\n`);
  if (diffs.some((d) => d.status !== "same")) exitCode = 1;
}
process.exit(exitCode);
