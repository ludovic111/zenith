#!/usr/bin/env node
/**
 * The Node oracle (plan §9.1): decodes JSON values with the real contracts, the way the web client
 * does, so Rust can check its encodings against TypeScript.
 *
 * Input: JSON Lines on stdin, one value per line:
 *   {"tag":"server.getConfig","kind":"success","json":{…}}     unary success value
 *   {"tag":"server.getConfig","kind":"error","json":{"_tag":"…"}}   a typed error (Fail cause)
 *   {"tag":"server.getConfig","kind":"exit","json":{"_tag":"Success","value":…}}   a whole Exit
 *   {"tag":"orchestration.subscribeShell","kind":"chunk","json":{…}}   one stream item
 *   {"tag":"orchestration.subscribeShell","kind":"payload","json":{…}}  a request payload
 *   {"http":"GET /api/auth/session","status":200,"json":{…}}   an HTTP response body
 *   {"http":"POST /api/orchestration/dispatch","kind":"payload","json":{…}}   an HTTP request body
 *
 * Output: one JSON line per input, {"line":n,"ok":true} or {"line":n,"ok":false,"message":"…"}.
 * Exit code 1 if any line failed. Usage:
 *   cargo test … --nocapture | node scripts/compat/oracle.ts
 *   node scripts/compat/oracle.ts < fixtures.jsonl
 */
import * as NodeReadline from "node:readline";
import { findHttpEndpoint, getRpcSpec, type DecodeOutcome } from "./lib/contracts.ts";

interface OracleLine {
  readonly tag?: string;
  readonly http?: string;
  readonly kind?: "payload" | "success" | "error" | "exit" | "chunk";
  readonly status?: number;
  readonly json: unknown;
}

const check = (input: OracleLine): DecodeOutcome => {
  if (input.http) {
    const [method = "GET", rawPath = "/"] = input.http.split(" ");
    const url = new URL(rawPath, "http://x.invalid");
    const found = findHttpEndpoint(method, url.pathname);
    if (!found) return { ok: false, message: `unknown HTTP endpoint ${input.http}` };
    if (input.kind === "payload") {
      const [codec] = [...found.spec.payload.values()];
      return codec ? codec.decode(input.json) : { ok: false, message: "endpoint takes no payload" };
    }
    const status = input.status ?? 200;
    const codec = found.spec.successes.get(status) ?? found.spec.errors.get(status);
    return codec
      ? codec.decode(input.json)
      : { ok: false, message: `status ${status} not declared` };
  }
  const spec = input.tag ? getRpcSpec(input.tag) : undefined;
  if (!spec) return { ok: false, message: `unknown RPC tag ${String(input.tag)}` };
  switch (input.kind) {
    case "payload":
      return spec.payload.decode(input.json);
    case "chunk":
      return spec.chunk
        ? spec.chunk.decode([input.json])
        : { ok: false, message: `${spec.tag} is not a stream` };
    case "exit":
      return spec.exit.decode(input.json);
    case "error":
      return spec.exit.decode({ _tag: "Failure", cause: [{ _tag: "Fail", error: input.json }] });
    case "success":
    default:
      return spec.isStream
        ? { ok: false, message: `${spec.tag} is a stream: use kind "chunk"` }
        : spec.exit.decode({ _tag: "Success", value: input.json });
  }
};

let failed = false;
let n = 0;
const rl = NodeReadline.createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of rl) {
  n++;
  if (line.trim().length === 0) continue;
  let outcome: DecodeOutcome;
  try {
    outcome = check(JSON.parse(line) as OracleLine);
  } catch (error) {
    outcome = { ok: false, message: `bad input line: ${String(error)}` };
  }
  if (!outcome.ok) failed = true;
  process.stdout.write(
    `${JSON.stringify(outcome.ok ? { line: n, ok: true } : { line: n, ok: false, message: outcome.message })}\n`,
  );
}
process.exit(failed ? 1 : 0);
