#!/usr/bin/env node
/**
 * Validates a recording offline with the same validators the proxy runs live: every
 * server→client WebSocket frame against the RPC contracts, every API HTTP response against its
 * endpoint schema.
 *
 *   node scripts/compat/validate.ts scripts/compat/recordings/*.jsonl [--strict] [--quiet]
 *
 * Exit code 1 if any recording has an error-level issue.
 */
import * as NodeUtil from "node:util";
import { formatIssue, summarizeIssues } from "./lib/proxy.ts";
import { readRecording } from "./lib/recording.ts";
import { validateHttpExchange, WsConnectionValidator, type Issue } from "./lib/validator.ts";

const { values, positionals } = NodeUtil.parseArgs({
  allowPositionals: true,
  options: {
    strict: { type: "boolean", default: false },
    quiet: { type: "boolean", default: false },
  },
});
if (positionals.length === 0) {
  process.stderr.write(
    "usage: node scripts/compat/validate.ts <recording.jsonl>… [--strict] [--quiet]\n",
  );
  process.exit(2);
}

let failed = false;
for (const file of positionals) {
  const issues: Array<Issue> = [];
  const validators = new Map<string, WsConnectionValidator>();
  const httpRequests = new Map<string, Record<string, unknown>>();
  const wsPaths = new Map<string, string>();
  for (const event of readRecording(file)) {
    const frame = event.frame as Record<string, unknown>;
    if (event.conn.startsWith("http")) {
      if (event.dir === "c2s") httpRequests.set(event.conn, frame);
      if (event.dir === "s2c") {
        const request = httpRequests.get(event.conn);
        if (!request) continue;
        const reqHeaders = (request.headers ?? {}) as Record<string, string>;
        const resHeaders = (frame.headers ?? {}) as Record<string, string>;
        const asText = (body: unknown) =>
          body === undefined ? undefined : typeof body === "string" ? body : JSON.stringify(body);
        issues.push(
          ...validateHttpExchange(
            {
              conn: event.conn,
              method: String(request.method),
              url: String(request.url),
              requestContentType: reqHeaders["content-type"],
              requestBody: asText(request.body),
              status: Number(frame.status),
              responseContentType: resHeaders["content-type"],
              responseBody: asText(frame.body),
            },
            { strict: values.strict },
          ).issues,
        );
      }
      continue;
    }
    if (event.dir === "open") {
      wsPaths.set(event.conn, String((frame as { path?: string }).path ?? "/ws"));
      continue;
    }
    if (event.dir !== "c2s" && event.dir !== "s2c") continue;
    if ((wsPaths.get(event.conn) ?? "/ws") !== "/ws") continue;
    let validator = validators.get(event.conn);
    if (!validator) {
      validator = new WsConnectionValidator(event.conn, { strict: values.strict });
      validators.set(event.conn, validator);
    }
    const text = JSON.stringify(frame);
    issues.push(
      ...(event.dir === "c2s" ? validator.onClientFrame(text) : validator.onServerFrame(text)),
    );
  }
  const summary = summarizeIssues(issues);
  if (!values.quiet) for (const issue of issues) process.stderr.write(`${formatIssue(issue)}\n`);
  process.stdout.write(`${file}: ${summary.errors} errors, ${summary.warnings} warnings\n`);
  if (summary.errors > 0) failed = true;
}
process.exit(failed ? 1 : 0);
