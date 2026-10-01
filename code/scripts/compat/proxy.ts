#!/usr/bin/env node
/**
 * Validating, recording proxy between apps/web (or any client) and a zenith code backend.
 *
 *   node scripts/compat/proxy.ts --target http://127.0.0.1:4749 --port 4790 \
 *     [--record session.jsonl] [--no-redact] [--strict] [--no-validate] \
 *     [--static ../apps/server/dist/client] [--quiet]
 *
 * Then open http://127.0.0.1:4790/ (pair with /pair#token=… from the backend's banner, or let the
 * dashboard's iframe point at the proxy). Issues are printed as they happen; Ctrl-C prints a
 * summary and exits non-zero if any error was seen.
 */
import * as NodeUtil from "node:util";
import { formatIssue, startProxy, summarizeIssues } from "./lib/proxy.ts";

const { values } = NodeUtil.parseArgs({
  options: {
    target: { type: "string" },
    host: { type: "string", default: "127.0.0.1" },
    port: { type: "string", default: "4790" },
    record: { type: "string" },
    "no-redact": { type: "boolean", default: false },
    strict: { type: "boolean", default: false },
    "no-validate": { type: "boolean", default: false },
    static: { type: "string" },
    quiet: { type: "boolean", default: false },
    help: { type: "boolean", short: "h", default: false },
  },
});

if (values.help || !values.target) {
  process.stdout.write(
    [
      "usage: node scripts/compat/proxy.ts --target <backend origin> [--port 4790] [--host 127.0.0.1]",
      "         [--record file.jsonl] [--no-redact] [--strict] [--no-validate] [--static <dir>] [--quiet]",
      "",
    ].join("\n"),
  );
  process.exit(values.help ? 0 : 2);
}

const proxy = await startProxy({
  target: values.target,
  host: values.host,
  port: Number(values.port),
  validate: !values["no-validate"],
  strict: values.strict,
  ...(values.record ? { recordFile: values.record } : {}),
  redact: !values["no-redact"],
  ...(values.static ? { staticDir: values.static } : {}),
  onIssue: (issue) => {
    if (!values.quiet || issue.severity === "error")
      process.stderr.write(`${formatIssue(issue)}\n`);
  },
});

process.stdout.write(
  `compat proxy: ${proxy.url} → ${values.target}` +
    `${values.record ? ` (recording to ${values.record})` : ""}` +
    `${values["no-validate"] ? "" : " (validating)"}\n`,
);

let stopping = false;
const stop = async () => {
  if (stopping) return;
  stopping = true;
  await proxy.close();
  const summary = summarizeIssues(proxy.issues);
  process.stdout.write(
    `${JSON.stringify(
      {
        http: proxy.stats.http,
        ws: proxy.stats.ws,
        wsClientFrames: proxy.stats.wsClientFrames,
        wsServerFrames: proxy.stats.wsServerFrames,
        endpoints: Object.fromEntries(proxy.stats.endpoints),
        rpcTags: Object.fromEntries(proxy.stats.rpcTags),
        ...summary,
      },
      null,
      2,
    )}\n`,
  );
  process.exit(summary.errors > 0 ? 1 : 0);
};
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
